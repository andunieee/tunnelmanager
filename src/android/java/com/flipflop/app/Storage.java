package com.flipflop.app;

import android.Manifest;
import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.media.MediaScannerConnection;
import android.net.Uri;
import android.os.Build;
import android.os.Environment;
import android.util.Log;
import android.webkit.MimeTypeMap;
import java.io.File;
import java.util.ArrayList;

/**
 * Received files in the public Downloads folder, and handing them (or
 * pasted text) to other apps.
 *
 * Files are exposed as MediaStore content URIs (via a scan) rather than
 * file:// URIs, which Android refuses to share across apps. Compiled into
 * the embedded dex like the other helpers (see src/android.rs).
 */
public class Storage {
    private static final String TAG = "flipflop-storage";
    private static final int PERMISSION_REQUEST = 0x7a13;
    private static boolean asked = false;

    /**
     * The public Downloads folder, or null when this app may not write there:
     * Android 10 (scoped storage without the legacy flag cargo-apk can't set),
     * or Android 8/9 before the storage permission is granted (asked once).
     */
    public static String downloadsDir(Activity activity) {
        int sdk = Build.VERSION.SDK_INT;
        if (sdk == Build.VERSION_CODES.Q) {
            return null;
        }
        if (sdk < Build.VERSION_CODES.Q
                && activity.checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE)
                        != PackageManager.PERMISSION_GRANTED) {
            if (!asked) {
                asked = true;
                activity.runOnUiThread(() -> activity.requestPermissions(
                        new String[] {Manifest.permission.WRITE_EXTERNAL_STORAGE},
                        PERMISSION_REQUEST));
            }
            return null;
        }
        File dir = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS);
        return dir == null ? null : dir.getAbsolutePath();
    }

    /** Opens `path` in the app the user picks; falls back to the share sheet. */
    public static void open(Activity activity, String path) {
        scan(activity, new String[] {path}, uris -> {
            Intent intent = new Intent(Intent.ACTION_VIEW);
            intent.setDataAndType(uris.get(0), mimeOf(path));
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
            try {
                activity.startActivity(intent);
            } catch (ActivityNotFoundException e) {
                activity.startActivity(Intent.createChooser(sendIntent(uris, new String[] {path}), null));
            }
        });
    }

    /** Offers `paths` to other apps through the share sheet. */
    public static void share(Activity activity, String[] paths) {
        scan(activity, paths, uris ->
                activity.startActivity(Intent.createChooser(sendIntent(uris, paths), null)));
    }

    /** Offers plain text to other apps through the share sheet. */
    public static void shareText(Activity activity, String text) {
        Intent intent = new Intent(Intent.ACTION_SEND);
        intent.setType("text/plain");
        intent.putExtra(Intent.EXTRA_TEXT, text);
        activity.runOnUiThread(() -> activity.startActivity(Intent.createChooser(intent, null)));
    }

    private static Intent sendIntent(ArrayList<Uri> uris, String[] paths) {
        Intent intent;
        if (uris.size() == 1) {
            intent = new Intent(Intent.ACTION_SEND);
            intent.setType(mimeOf(paths[0]));
            intent.putExtra(Intent.EXTRA_STREAM, uris.get(0));
        } else {
            intent = new Intent(Intent.ACTION_SEND_MULTIPLE);
            intent.setType("*/*");
            intent.putParcelableArrayListExtra(Intent.EXTRA_STREAM, uris);
        }
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        return intent;
    }

    private interface OnScanned {
        void run(ArrayList<Uri> uris);
    }

    /** Resolves files to content URIs, then runs `done` on the UI thread. */
    private static void scan(Activity activity, String[] paths, OnScanned done) {
        ArrayList<Uri> uris = new ArrayList<>();
        int[] pending = {paths.length};
        MediaScannerConnection.scanFile(activity, paths, null, (path, uri) -> {
            synchronized (uris) {
                if (uri != null) {
                    uris.add(uri);
                } else {
                    Log.w(TAG, "no content uri for a file");
                }
                if (--pending[0] > 0) {
                    return;
                }
            }
            if (uris.isEmpty()) {
                return;
            }
            activity.runOnUiThread(() -> {
                try {
                    done.run(uris);
                } catch (RuntimeException e) {
                    Log.w(TAG, "could not hand files over", e);
                }
            });
        });
    }

    private static String mimeOf(String path) {
        int dot = path.lastIndexOf('.');
        if (dot >= 0) {
            String type = MimeTypeMap.getSingleton()
                    .getMimeTypeFromExtension(path.substring(dot + 1).toLowerCase());
            if (type != null) {
                return type;
            }
        }
        return "*/*";
    }
}
