package io.fasttran.app;

import android.content.ContentProvider;
import android.util.Base64;
import android.content.ContentValues;
import android.content.Context;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract;
import android.provider.OpenableColumns;

import java.io.File;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * Read-only content provider for files inside FastTran's app-specific
 * storage.  Android 7+ blocks raw file:// URIs, so received files are
 * exposed to the system file picker through this provider instead.
 */
public final class FastTranFileProvider extends ContentProvider {
    public static final String AUTHORITY = "io.fasttran.app.files";
    private static final String PATH_PREFIX = "open";
    private static final String MIME_DIRECTORY = DocumentsContract.Document.MIME_TYPE_DIR;

    public static Uri uriFor(Context context, File file) {
        String token = Base64.encodeToString(
                file.getAbsolutePath().getBytes(StandardCharsets.UTF_8),
                Base64.URL_SAFE | Base64.NO_WRAP | Base64.NO_PADDING);
        return new Uri.Builder()
                .scheme("content")
                .authority(AUTHORITY)
                .appendPath(PATH_PREFIX)
                .appendPath(token)
                .build();
    }

    public static String mimeFor(File file) {
        if (file.isDirectory()) {
            return MIME_DIRECTORY;
        }
        String mime = android.webkit.MimeTypeMap.getSingleton()
                .getMimeTypeFromExtension(extension(file));
        return mime == null ? "application/octet-stream" : mime;
    }

    @Override
    public boolean onCreate() {
        return true;
    }

    @Override
    public String getType(Uri uri) {
        return mimeFor(resolve(uri));
    }

    @Override
    public Cursor query(
            Uri uri,
            String[] projection,
            String selection,
            String[] selectionArgs,
            String sortOrder) {
        File file = resolve(uri);
        String[] columns = projection == null
                ? new String[]{OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE}
                : projection;
        MatrixCursor cursor = new MatrixCursor(columns, 1);
        Object[] values = new Object[columns.length];
        for (int index = 0; index < columns.length; index++) {
            String column = columns[index];
            if (OpenableColumns.DISPLAY_NAME.equals(column)) {
                values[index] = file.getName();
            } else if (OpenableColumns.SIZE.equals(column)) {
                values[index] = file.isDirectory() ? 0L : file.length();
            } else if ("mime_type".equals(column)) {
                values[index] = mimeFor(file);
            } else {
                values[index] = null;
            }
        }
        cursor.addRow(values);
        return cursor;
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        if (mode != null && !mode.equals("r") && !mode.equals("rt")) {
            throw new FileNotFoundException("FastTran files are read-only");
        }
        File file = resolve(uri);
        return ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY);
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        throw new UnsupportedOperationException("read-only provider");
    }

    @Override
    public int delete(Uri uri, String selection, String[] selectionArgs) {
        throw new UnsupportedOperationException("read-only provider");
    }

    @Override
    public int update(
            Uri uri,
            ContentValues values,
            String selection,
            String[] selectionArgs) {
        throw new UnsupportedOperationException("read-only provider");
    }

    private File resolve(Uri uri) {
        Context context = getContext();
        if (context == null) {
            throw new IllegalArgumentException("provider is not attached");
        }
        if (!"content".equals(uri.getScheme()) || !AUTHORITY.equals(uri.getAuthority())) {
            throw new IllegalArgumentException("unsupported URI");
        }

        List<String> segments = uri.getPathSegments();
        if (segments.size() != 2 || !PATH_PREFIX.equals(segments.get(0))) {
            throw new IllegalArgumentException("unsupported URI path");
        }

        final String path;
        try {
            path = new String(
                    Base64.decode(segments.get(1), Base64.URL_SAFE | Base64.NO_WRAP | Base64.NO_PADDING),
                    StandardCharsets.UTF_8);
        } catch (IllegalArgumentException error) {
            throw new IllegalArgumentException("invalid file token", error);
        }

        File file;
        try {
            file = new File(path).getCanonicalFile();
        } catch (IOException error) {
            throw new IllegalArgumentException("invalid file path", error);
        }
        if (!isAllowed(context, file)) {
            throw new SecurityException("file is outside FastTran storage");
        }
        if (!file.exists()) {
            throw new IllegalArgumentException("file does not exist");
        }
        return file;
    }

    private static boolean isAllowed(Context context, File file) {
        File[] roots = new File[]{
                context.getExternalFilesDir(null),
                context.getFilesDir(),
                context.getCacheDir()
        };
        String filePath = file.getPath();
        for (File root : roots) {
            if (root == null) {
                continue;
            }
            try {
                String rootPath = root.getCanonicalPath();
                if (filePath.equals(rootPath) || filePath.startsWith(rootPath + File.separator)) {
                    return true;
                }
            } catch (IOException ignored) {
                // Try the next root.
            }
        }
        return false;
    }

    private static String extension(File file) {
        String name = file.getName();
        int dot = name.lastIndexOf('.');
        return dot >= 0 && dot + 1 < name.length()
                ? name.substring(dot + 1).toLowerCase()
                : "";
    }
}
