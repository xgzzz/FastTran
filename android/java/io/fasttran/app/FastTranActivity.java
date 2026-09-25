package io.fasttran.app;

import android.app.NativeActivity;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.Intent;
import android.database.Cursor;
import android.graphics.Color;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.system.ErrnoException;
import android.system.Os;
import android.provider.DocumentsContract;
import android.provider.OpenableColumns;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Small Java shell around NativeActivity.
 *
 * NativeActivity does not forward activity results to Rust, and the Android
 * file manager cannot open a raw file:// URI on modern Android versions.  The
 * shell owns the document picker and uses FastTranFileProvider for safe,
 * read-only content URIs when opening received files.
 */
public class FastTranActivity extends NativeActivity {
    private static final int REQUEST_OPEN_DOCUMENT = 4101;
    private static final int EDGE_TO_EDGE_FLAGS =
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                    | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                    | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION;
    private static final Map<String, ParcelFileDescriptor> PICKED_DESCRIPTORS =
            new HashMap<>();

    private static native void nativeOnPickedFile(String path);
    private static native void nativeOnPickerError(String message);
    private static native void nativeOnSafeInsets(int top, int right, int bottom, int left);

    private final ExecutorService copyExecutor = Executors.newSingleThreadExecutor();
    private boolean pickerInProgress;

    static {
        // NativeActivity also loads this library from android.app.lib_name.
        // Loading it here makes the JNI callbacks available as soon as this
        // custom Activity class is used.
        System.loadLibrary("fasttran_core");
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        trimPickedCache();

        Window window = getWindow();
        window.setStatusBarColor(Color.TRANSPARENT);
        window.setNavigationBarColor(Color.TRANSPARENT);
        configureEdgeToEdge(window);

        View decor = window.getDecorView();
        decor.setOnApplyWindowInsetsListener((view, insets) -> {
            reportSafeInsets(insets);
            return insets;
        });
        decor.requestApplyInsets();
        applySystemBarStyle(false);
    }

    private void configureEdgeToEdge(Window window) {
        if (Build.VERSION.SDK_INT >= 30) {
            window.setDecorFitsSystemWindows(false);
        } else {
            window.getDecorView().setSystemUiVisibility(EDGE_TO_EDGE_FLAGS);
        }
    }

    private void reportSafeInsets(WindowInsets insets) {
        float density = getResources().getDisplayMetrics().density;
        if (density <= 0.0f) {
            density = 1.0f;
        }

        int top;
        int right;
        int bottom;
        int left;
        if (Build.VERSION.SDK_INT >= 30) {
            android.graphics.Insets bars = insets.getInsets(
                    WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
            android.graphics.Insets status = insets.getInsets(WindowInsets.Type.statusBars());
            android.graphics.Insets cutout = insets.getInsets(WindowInsets.Type.displayCutout());
            android.graphics.Insets ime = insets.getInsets(WindowInsets.Type.ime());
            top = Math.max(status.top, cutout.top);
            right = bars.right;
            bottom = Math.max(bars.bottom, ime.bottom);
            left = bars.left;
        } else {
            top = insets.getSystemWindowInsetTop();
            right = insets.getSystemWindowInsetRight();
            bottom = insets.getSystemWindowInsetBottom();
            left = insets.getSystemWindowInsetLeft();
            if (Build.VERSION.SDK_INT >= 28 && insets.getDisplayCutout() != null) {
                android.view.DisplayCutout cutout = insets.getDisplayCutout();
                top = Math.max(top, cutout.getSafeInsetTop());
                right = Math.max(right, cutout.getSafeInsetRight());
                bottom = Math.max(bottom, cutout.getSafeInsetBottom());
                left = Math.max(left, cutout.getSafeInsetLeft());
            }
        }

        nativeOnSafeInsets(
                Math.round(top / density),
                Math.round(right / density),
                Math.round(bottom / density),
                Math.round(left / density));
    }

    public void applySystemBarStyle(boolean darkMode) {
        if (Build.VERSION.SDK_INT >= 30) {
            WindowInsetsController controller = getWindow().getInsetsController();
            if (controller != null) {
                int lightBars = WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                        | WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS;
                controller.setSystemBarsAppearance(darkMode ? 0 : lightBars, lightBars);
            }
        } else {
            int flags = EDGE_TO_EDGE_FLAGS;
            if (!darkMode) {
                flags |= View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                        | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR;
            }
            getWindow().getDecorView().setSystemUiVisibility(flags);
        }
    }

    /** Launch Android's standard multi-document picker. */
    public void chooseFiles() {
        if (pickerInProgress) {
            return;
        }

        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        intent.setType("*/*");
        intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
        try {
            pickerInProgress = true;
            startActivityForResult(intent, REQUEST_OPEN_DOCUMENT);
        } catch (ActivityNotFoundException error) {
            pickerInProgress = false;
            nativeOnPickerError("No file picker is available");
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != REQUEST_OPEN_DOCUMENT) {
            return;
        }

        pickerInProgress = false;
        if (resultCode != RESULT_OK || data == null) {
            return;
        }

        LinkedHashSet<Uri> uris = new LinkedHashSet<>();
        ClipData clipData = data.getClipData();
        if (clipData != null) {
            for (int index = 0; index < clipData.getItemCount(); index++) {
                Uri uri = clipData.getItemAt(index).getUri();
                if (uri != null) {
                    uris.add(uri);
                }
            }
        }

        ArrayList<Uri> streamUris = data.getParcelableArrayListExtra(Intent.EXTRA_STREAM);
        if (streamUris != null) {
            uris.addAll(streamUris);
        }
        Object stream = data.getParcelableExtra(Intent.EXTRA_STREAM);
        if (stream instanceof Uri) {
            uris.add((Uri) stream);
        }
        if (data.getData() != null) {
            uris.add(data.getData());
        }

        if (uris.isEmpty()) {
            nativeOnPickerError("The selected document did not contain a readable file");
            return;
        }
        for (Uri uri : uris) {
            copyExecutor.execute(() -> copyPickedUri(uri));
        }
    }

    private void copyPickedUri(Uri uri) {
        try {
            // Keep a live file descriptor and expose it through a private
            // symlink. Rust can open /proc/self/fd/<n> without copying the
            // document into a second cache file.
            nativeOnPickedFile(exposeUri(uri));
        } catch (Exception directError) {
            try {
                nativeOnPickedFile(copyUriToPrivateStorage(uri));
            } catch (Exception copyError) {
                String message = copyError.getMessage();
                nativeOnPickerError(message == null || message.isEmpty()
                        ? "Unable to read the selected file"
                        : message);
            }
        }
    }

    private String exposeUri(Uri uri) throws IOException {
        ParcelFileDescriptor descriptor = getContentResolver().openFileDescriptor(uri, "r");
        if (descriptor == null) {
            throw new IOException("The selected file could not be opened");
        }

        boolean linked = false;
        try {
            String displayName = queryDisplayName(uri);
            if (displayName == null || displayName.trim().isEmpty()) {
                displayName = "selected-file";
            }
            displayName = sanitizeDisplayName(displayName);

            File directory = new File(getCacheDir(), "picked");
            if (!directory.exists() && !directory.mkdirs()) {
                throw new IOException("Unable to create the file cache directory");
            }
            File link = uniqueFile(directory, displayName);
            String target = "/proc/self/fd/" + descriptor.getFd();
            try {
                Os.symlink(target, link.getAbsolutePath());
                linked = true;
            } catch (ErrnoException error) {
                throw new IOException("Unable to expose the selected file", error);
            }

            synchronized (PICKED_DESCRIPTORS) {
                PICKED_DESCRIPTORS.put(link.getAbsolutePath(), descriptor);
            }
            return link.getAbsolutePath();
        } finally {
            if (!linked) {
                try {
                    descriptor.close();
                } catch (IOException ignored) {
                    // The original URI error is more useful to the user.
                }
            }
        }
    }

    public void releasePickedFile(String path) {
        ParcelFileDescriptor descriptor;
        synchronized (PICKED_DESCRIPTORS) {
            descriptor = PICKED_DESCRIPTORS.remove(path);
        }
        // The Rust side normally removes the symlink first; remove it here as
        // well for callers that release a path directly.
        //noinspection ResultOfMethodCallIgnored
        new File(path).delete();
        if (descriptor != null) {
            try {
                descriptor.close();
            } catch (IOException ignored) {
                // The file descriptor is being discarded; nothing else to do.
            }
        }
    }

    private void trimPickedCache() {
        File directory = new File(getCacheDir(), "picked");
        File[] files = directory.listFiles();
        if (files == null) {
            return;
        }
        long cutoff = System.currentTimeMillis() - 24L * 60L * 60L * 1000L;
        for (File file : files) {
            if (file.lastModified() < cutoff) {
                //noinspection ResultOfMethodCallIgnored
                file.delete();
            }
        }
    }

    private String copyUriToPrivateStorage(Uri uri) throws IOException {
        trimPickedCache();
        String displayName = queryDisplayName(uri);
        if (displayName == null || displayName.trim().isEmpty()) {
            displayName = "selected-file";
        }
        displayName = sanitizeDisplayName(displayName);

        File directory = new File(getCacheDir(), "picked");
        if (!directory.exists() && !directory.mkdirs()) {
            throw new IOException("Unable to create the file cache directory");
        }

        File destination = uniqueFile(directory, displayName);
        try (InputStream input = getContentResolver().openInputStream(uri);
             FileOutputStream output = new FileOutputStream(destination)) {
            if (input == null) {
                throw new IOException("The selected file has no readable stream");
            }
            byte[] buffer = new byte[2 * 1024 * 1024];
            int count;
            while ((count = input.read(buffer)) >= 0) {
                if (count > 0) {
                    output.write(buffer, 0, count);
                }
            }
            output.flush();
        } catch (IOException error) {
            // Do not leave a partial file that the sender might later pick up.
            //noinspection ResultOfMethodCallIgnored
            destination.delete();
            throw error;
        }
        return destination.getAbsolutePath();
    }

    private String queryDisplayName(Uri uri) {
        Cursor cursor = null;
        try {
            cursor = getContentResolver().query(
                    uri,
                    new String[]{OpenableColumns.DISPLAY_NAME},
                    null,
                    null,
                    null);
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (column >= 0) {
                    return cursor.getString(column);
                }
            }
        } catch (RuntimeException ignored) {
            // Some document providers do not implement query(); use the URI
            // segment as a fallback below.
        } finally {
            if (cursor != null) {
                cursor.close();
            }
        }

        String segment = uri.getLastPathSegment();
        return segment == null ? null : segment;
    }

    private static String sanitizeDisplayName(String name) {
        String value = name.replaceAll("[\\\\/:*?\"<>|\\p{Cntrl}]", "_").trim();
        if (value.isEmpty() || ".".equals(value) || "..".equals(value)) {
            value = "selected-file";
        }
        if (value.length() > 120) {
            value = value.substring(value.length() - 120);
        }
        return value;
    }

    private static File uniqueFile(File directory, String name) {
        File candidate = new File(directory, name);
        if (!candidate.exists()) {
            return candidate;
        }

        int dot = name.lastIndexOf('.');
        String stem = dot > 0 ? name.substring(0, dot) : name;
        String extension = dot > 0 ? name.substring(dot) : "";
        return new File(
                directory,
                stem + "-" + UUID.randomUUID().toString() + extension);
    }

    /** Open a received file (or a directory) with a read-only content URI. */
    public void openPath(String path) {
        File target = new File(path);
        Uri contentUri = FastTranFileProvider.uriFor(this, target);
        String mime = FastTranFileProvider.mimeFor(target);
        Intent view = new Intent(Intent.ACTION_VIEW);
        view.setDataAndType(contentUri, mime);
        view.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        try {
            startActivity(view);
        } catch (ActivityNotFoundException | SecurityException error) {
            if (target.isDirectory()) {
                openDirectoryChooser(target);
            } else {
                nativeOnPickerError("No app can open this file");
            }
        }
    }

    private Uri externalStorageTreeUri(File directory) {
        String path = directory.getAbsolutePath();
        String prefix = "/storage/emulated/0/";
        if (!path.startsWith(prefix)) {
            return null;
        }
        String relative = path.substring(prefix.length());
        if (relative.isEmpty()) {
            relative = ".";
        }
        String documentId = "primary:" + relative;
        return DocumentsContract.buildTreeDocumentUri(
                "com.android.externalstorage.documents", documentId);
    }

    private void openDirectoryChooser(File directory) {
        try {
            Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE);
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
            Uri initial = externalStorageTreeUri(directory);
            if (initial != null) {
                intent.putExtra("android.provider.extra.INITIAL_URI", initial);
            }
            startActivity(intent);
        } catch (ActivityNotFoundException | SecurityException error) {
            nativeOnPickerError("No app can open this folder");
        }
    }

    @Override
    protected void onDestroy() {
        copyExecutor.shutdownNow();
        super.onDestroy();
    }
}
