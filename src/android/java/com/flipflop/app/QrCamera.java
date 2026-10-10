package com.flipflop.app;

import android.Manifest;
import android.app.Activity;
import android.content.Context;
import android.content.pm.PackageManager;
import android.graphics.ImageFormat;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCaptureSession;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraDevice;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.CaptureRequest;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.Image;
import android.media.ImageReader;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.util.Log;
import android.util.Size;
import java.nio.ByteBuffer;
import java.util.Collections;

/**
 * Camera frames for scanning a peer's QR code.
 *
 * Streams the back camera's luma plane to native code ({@link #onFrame}),
 * which decodes it and shows it as the viewfinder; there is no camera
 * preview surface of our own. {@link #onStopped} reports that the camera
 * went away (or could not start), so the UI can close the scanner. Loaded
 * from the embedded dex like the other helpers (see src/android.rs).
 */
public final class QrCamera {
    private static final String TAG = "flipflop-qr";
    private static final int PERMISSION_REQUEST = 0x7a14;
    private static final long PERMISSION_POLL_MS = 500;
    /** Give up waiting for the permission dialog after this long. */
    private static final long PERMISSION_WAIT_MS = 60_000;
    /** At most this many frames per second go to native code. */
    private static final long FRAME_MS = 100;
    private static final int WIDTH = 640;
    private static final int HEIGHT = 480;
    private static final Handler main = new Handler(Looper.getMainLooper());

    /** `luma` is width x height bytes; rotate clockwise by `rotation` to show it upright. */
    static native void onFrame(byte[] luma, int width, int height, int rotation);

    /** `denied`: the camera permission was refused. */
    static native void onStopped(boolean denied);

    private static Activity activity;
    private static boolean wanted;
    private static HandlerThread thread;
    private static CameraDevice device;
    private static CameraCaptureSession session;
    private static ImageReader reader;
    private static long lastFrame;

    private QrCamera() {}

    /** Starts streaming, asking for the camera permission first. UI thread. */
    public static void start(Activity a) {
        activity = a;
        wanted = true;
        if (a.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            open();
            return;
        }
        // NativeActivity drops permission results: ask, then poll for the grant.
        a.requestPermissions(new String[] {Manifest.permission.CAMERA}, PERMISSION_REQUEST);
        long deadline = System.currentTimeMillis() + PERMISSION_WAIT_MS;
        main.postDelayed(() -> awaitPermission(deadline), PERMISSION_POLL_MS);
    }

    private static void awaitPermission(long deadline) {
        if (!wanted) {
            return;
        }
        if (activity.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            open();
        } else if (System.currentTimeMillis() > deadline) {
            wanted = false;
            onStopped(true);
        } else {
            main.postDelayed(() -> awaitPermission(deadline), PERMISSION_POLL_MS);
        }
    }

    /** Stops streaming. Any thread. */
    public static void stop() {
        main.post(() -> {
            wanted = false;
            close();
        });
    }

    private static void open() {
        CameraManager manager = (CameraManager) activity.getSystemService(Context.CAMERA_SERVICE);
        try {
            String id = backCamera(manager);
            if (id == null) {
                Log.w(TAG, "no camera");
                fail();
                return;
            }
            CameraCharacteristics info = manager.getCameraCharacteristics(id);
            Integer sensor = info.get(CameraCharacteristics.SENSOR_ORIENTATION);
            int rotation = sensor == null ? 0 : sensor;
            Size size = frameSize(info);
            thread = new HandlerThread("qr-camera");
            thread.start();
            Handler handler = new Handler(thread.getLooper());
            reader = ImageReader.newInstance(size.getWidth(), size.getHeight(), ImageFormat.YUV_420_888, 2);
            reader.setOnImageAvailableListener(r -> frame(r, rotation), handler);
            manager.openCamera(id, new CameraDevice.StateCallback() {
                @Override
                public void onOpened(CameraDevice camera) {
                    device = camera;
                    if (!wanted) {
                        main.post(QrCamera::close);
                        return;
                    }
                    startSession(camera, handler);
                }

                @Override
                public void onDisconnected(CameraDevice camera) {
                    camera.close();
                    fail();
                }

                @Override
                public void onError(CameraDevice camera, int error) {
                    Log.w(TAG, "camera error " + error);
                    camera.close();
                    fail();
                }
            }, handler);
        } catch (CameraAccessException | SecurityException | IllegalArgumentException e) {
            Log.w(TAG, "cannot open the camera", e);
            fail();
        }
    }

    @SuppressWarnings("deprecation") // the List<Surface> overload is the one API 26 has
    private static void startSession(CameraDevice camera, Handler handler) {
        try {
            camera.createCaptureSession(
                    Collections.singletonList(reader.getSurface()),
                    new CameraCaptureSession.StateCallback() {
                        @Override
                        public void onConfigured(CameraCaptureSession s) {
                            session = s;
                            try {
                                CaptureRequest.Builder request =
                                        camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW);
                                request.addTarget(reader.getSurface());
                                request.set(
                                        CaptureRequest.CONTROL_AF_MODE,
                                        CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE);
                                s.setRepeatingRequest(request.build(), null, handler);
                            } catch (CameraAccessException | IllegalStateException e) {
                                Log.w(TAG, "cannot start the camera", e);
                                fail();
                            }
                        }

                        @Override
                        public void onConfigureFailed(CameraCaptureSession s) {
                            Log.w(TAG, "camera session failed");
                            fail();
                        }
                    },
                    handler);
        } catch (CameraAccessException | IllegalStateException e) {
            Log.w(TAG, "cannot configure the camera", e);
            fail();
        }
    }

    private static void frame(ImageReader r, int rotation) {
        Image image;
        try {
            image = r.acquireLatestImage();
        } catch (IllegalStateException e) {
            return; // closed by stop()
        }
        if (image == null) {
            return;
        }
        try {
            long now = System.currentTimeMillis();
            if (now - lastFrame < FRAME_MS) {
                return;
            }
            lastFrame = now;
            int width = image.getWidth();
            int height = image.getHeight();
            Image.Plane plane = image.getPlanes()[0];
            ByteBuffer buffer = plane.getBuffer();
            int rowStride = plane.getRowStride();
            int pixelStride = plane.getPixelStride();
            byte[] luma = new byte[width * height];
            if (pixelStride == 1 && rowStride == width) {
                buffer.get(luma, 0, Math.min(luma.length, buffer.remaining()));
            } else {
                byte[] row = new byte[rowStride];
                for (int y = 0; y < height; y++) {
                    buffer.position(y * rowStride);
                    int length = Math.min(rowStride, buffer.remaining());
                    buffer.get(row, 0, length);
                    for (int x = 0; x < width && x * pixelStride < length; x++) {
                        luma[y * width + x] = row[x * pixelStride];
                    }
                }
            }
            onFrame(luma, width, height, rotation);
        } catch (RuntimeException e) {
            Log.w(TAG, "bad frame", e);
        } finally {
            image.close();
        }
    }

    private static String backCamera(CameraManager manager) throws CameraAccessException {
        String fallback = null;
        for (String id : manager.getCameraIdList()) {
            Integer facing = manager.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING);
            if (facing != null && facing == CameraCharacteristics.LENS_FACING_BACK) {
                return id;
            }
            if (fallback == null) {
                fallback = id;
            }
        }
        return fallback;
    }

    /** The supported YUV size closest to 640x480 in pixel count. */
    private static Size frameSize(CameraCharacteristics info) {
        StreamConfigurationMap map = info.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
        Size best = new Size(WIDTH, HEIGHT);
        if (map == null) {
            return best;
        }
        Size[] sizes = map.getOutputSizes(ImageFormat.YUV_420_888);
        if (sizes == null || sizes.length == 0) {
            return best;
        }
        long target = (long) WIDTH * HEIGHT;
        long bestDiff = Long.MAX_VALUE;
        for (Size size : sizes) {
            long diff = Math.abs((long) size.getWidth() * size.getHeight() - target);
            if (diff < bestDiff) {
                bestDiff = diff;
                best = size;
            }
        }
        return best;
    }

    private static void fail() {
        main.post(() -> {
            boolean was = wanted;
            wanted = false;
            close();
            if (was) {
                onStopped(false);
            }
        });
    }

    private static void close() {
        if (session != null) {
            try {
                session.close();
            } catch (RuntimeException e) {
                // closing anyway
            }
            session = null;
        }
        if (device != null) {
            device.close();
            device = null;
        }
        if (reader != null) {
            reader.close();
            reader = null;
        }
        if (thread != null) {
            thread.quitSafely();
            thread = null;
        }
    }
}
