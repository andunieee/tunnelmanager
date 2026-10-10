package com.flipflop.app;

import android.app.Activity;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothServerSocket;
import android.bluetooth.BluetoothSocket;
import android.bluetooth.le.AdvertiseCallback;
import android.bluetooth.le.AdvertiseData;
import android.bluetooth.le.AdvertiseSettings;
import android.bluetooth.le.BluetoothLeAdvertiser;
import android.bluetooth.le.BluetoothLeScanner;
import android.bluetooth.le.ScanCallback;
import android.bluetooth.le.ScanFilter;
import android.bluetooth.le.ScanRecord;
import android.bluetooth.le.ScanResult;
import android.bluetooth.le.ScanSettings;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.location.LocationManager;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.os.ParcelUuid;
import android.util.Log;
import android.widget.Toast;

import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.TimeUnit;

/**
 * Bluetooth links for the native iroh transport (src/native/src/bluetooth.rs).
 *
 * Each device advertises a BLE service carrying its peer tag (a prefix of
 * its endpoint id; Android hides the device's own Bluetooth address) and
 * the PSM of an L2CAP channel it listens on, and scans for the same service
 * from others. Packets for a peer go over an L2CAP channel to it, opened on
 * first use; each packet is a length-prefixed frame, and each side's first
 * frame is its own tag, so an accepted channel knows who it is from.
 *
 * Packets are datagrams to the transport above: a full send queue drops
 * them, as a busy UDP socket would. Native code registers the static
 * {@code on*} callbacks; the class is loaded from the embedded dex (see
 * src/android.rs).
 */
public final class BluetoothLink {
    private static final String TAG = "flipflop-bt";
    /** The service every flipflop device advertises. */
    private static final ParcelUuid SERVICE =
            new ParcelUuid(UUID.fromString("16db0b95-cf03-4873-b43b-9d9d2b807981"));
    /** Must match PEER_TAG_LEN in bluetooth.rs. */
    private static final int TAG_LEN = 6;
    private static final int QUEUE = 64;
    /** Tell native code about a peer at most this often. */
    private static final long SEEN_REPORT_MS = 10_000;
    /** After a failed dial, wait this long before dialing that peer again. */
    private static final long REDIAL_MS = 5_000;
    private static final int PERMISSION_REQUEST = 0x7b70;
    private static final long PERMISSION_POLL_MS = 2_000;
    private static final Handler main = new Handler(Looper.getMainLooper());

    static native void onPacket(byte[] from, byte[] packet);

    static native void onPeerSeen(byte[] tag);

    static native void onAvailable(boolean available);

    private static Context context;
    private static BluetoothAdapter adapter;
    private static byte[] localTag;
    private static volatile boolean up;
    private static BluetoothServerSocket server;
    private static AdvertiseCallback advertising;
    private static ScanCallback scanning;

    /** Peers seen in scans, by hex tag. */
    private static final Map<String, Sighting> seen = new ConcurrentHashMap<>();
    /** The channel packets for a peer go out on, by hex tag. */
    private static final Map<String, Link> links = new ConcurrentHashMap<>();
    /** When native code last heard of a peer, by hex tag. */
    private static final Map<String, Long> reported = new ConcurrentHashMap<>();
    /** When a dial to a peer last failed, by hex tag. */
    private static final Map<String, Long> dialFailed = new ConcurrentHashMap<>();

    private BluetoothLink() {}

    /**
     * Whether this device can carry the transport: Android 10+ (LE L2CAP
     * channels) and a Bluetooth LE adapter. Says nothing about whether it
     * is switched on.
     */
    public static boolean supported(Activity activity) {
        return Build.VERSION.SDK_INT >= 29
                && activity.getPackageManager()
                        .hasSystemFeature(PackageManager.FEATURE_BLUETOOTH_LE)
                && BluetoothAdapter.getDefaultAdapter() != null;
    }

    /**
     * Asks for the permissions Bluetooth needs, then starts advertising,
     * scanning and listening whenever the adapter is on. UI thread.
     */
    public static void start(Activity activity, byte[] tag) {
        context = activity.getApplicationContext();
        adapter = BluetoothAdapter.getDefaultAdapter();
        localTag = tag.clone();
        List<String> missing = new ArrayList<>();
        for (String permission : neededPermissions()) {
            if (activity.checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
                missing.add(permission);
            }
        }
        if (missing.isEmpty()) {
            begin();
            return;
        }
        // NativeActivity drops permission results, and this runs before the
        // activity can host a fragment to catch them: ask, then watch for
        // the grant. That also catches a grant made later in Settings.
        activity.requestPermissions(missing.toArray(new String[0]), PERMISSION_REQUEST);
        onAvailable(false);
        main.postDelayed(BluetoothLink::awaitPermissions, PERMISSION_POLL_MS);
    }

    private static void awaitPermissions() {
        for (String permission : neededPermissions()) {
            if (context.checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
                main.postDelayed(BluetoothLink::awaitPermissions, PERMISSION_POLL_MS);
                return;
            }
        }
        Log.i(TAG, "permissions granted");
        begin();
    }

    private static String[] neededPermissions() {
        if (Build.VERSION.SDK_INT >= 31) {
            // Scans also need location: the manifest can't declare
            // BLUETOOTH_SCAN "neverForLocation" (cargo-apk has no flag for it).
            return new String[] {
                "android.permission.BLUETOOTH_SCAN",
                "android.permission.BLUETOOTH_ADVERTISE",
                "android.permission.BLUETOOTH_CONNECT",
                "android.permission.ACCESS_FINE_LOCATION",
                "android.permission.ACCESS_COARSE_LOCATION",
            };
        }
        return new String[] {"android.permission.ACCESS_FINE_LOCATION"};
    }

    /** Follow the adapter: up while it is on, down otherwise. */
    private static void begin() {
        IntentFilter filter = new IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED);
        context.registerReceiver(
                new BroadcastReceiver() {
                    @Override
                    public void onReceive(Context c, Intent intent) {
                        int state = intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, -1);
                        if (state == BluetoothAdapter.STATE_ON) {
                            bringUp();
                        } else if (state == BluetoothAdapter.STATE_TURNING_OFF
                                || state == BluetoothAdapter.STATE_OFF) {
                            bringDown();
                        }
                    }
                },
                filter);
        if (adapter.isEnabled()) {
            bringUp();
        } else {
            onAvailable(false);
        }
    }

    private static synchronized void bringUp() {
        if (up) {
            return;
        }
        int psm;
        try {
            server = adapter.listenUsingInsecureL2capChannel();
            psm = server.getPsm();
        } catch (IOException | SecurityException e) {
            Log.w(TAG, "cannot listen for L2CAP channels", e);
            onAvailable(false);
            return;
        }
        up = true;
        final BluetoothServerSocket listening = server;
        new Thread(() -> acceptLoop(listening), "bt-accept").start();
        advertise(psm);
        scan();
        onAvailable(true);
        Log.i(TAG, "up, psm " + psm);
    }

    private static synchronized void bringDown() {
        if (!up) {
            return;
        }
        up = false;
        onAvailable(false);
        try {
            BluetoothLeAdvertiser advertiser = adapter.getBluetoothLeAdvertiser();
            if (advertiser != null && advertising != null) {
                advertiser.stopAdvertising(advertising);
            }
            BluetoothLeScanner scanner = adapter.getBluetoothLeScanner();
            if (scanner != null && scanning != null) {
                scanner.stopScan(scanning);
            }
        } catch (RuntimeException e) {
            // The adapter is going away; its LE objects with it.
        }
        advertising = null;
        scanning = null;
        closeQuietly(server);
        server = null;
        for (Link link : links.values()) {
            link.close();
        }
        links.clear();
        seen.clear();
        reported.clear();
        dialFailed.clear();
        Log.i(TAG, "down");
    }

    private static void advertise(int psm) {
        BluetoothLeAdvertiser advertiser = adapter.getBluetoothLeAdvertiser();
        if (advertiser == null) {
            // Can still dial peers that advertise; they just can't find us.
            Log.w(TAG, "this device cannot advertise");
            return;
        }
        byte[] payload = new byte[TAG_LEN + 2];
        System.arraycopy(localTag, 0, payload, 0, TAG_LEN);
        payload[TAG_LEN] = (byte) (psm >> 8);
        payload[TAG_LEN + 1] = (byte) psm;
        AdvertiseSettings settings =
                new AdvertiseSettings.Builder()
                        .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_BALANCED)
                        .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_MEDIUM)
                        .setConnectable(true)
                        .setTimeout(0)
                        .build();
        AdvertiseData data =
                new AdvertiseData.Builder()
                        .setIncludeDeviceName(false)
                        .setIncludeTxPowerLevel(false)
                        .addServiceData(SERVICE, payload)
                        .build();
        advertising =
                new AdvertiseCallback() {
                    @Override
                    public void onStartFailure(int errorCode) {
                        Log.w(TAG, "advertising failed: " + errorCode);
                    }
                };
        try {
            advertiser.startAdvertising(settings, data, advertising);
        } catch (RuntimeException e) {
            Log.w(TAG, "cannot advertise", e);
        }
    }

    private static void scan() {
        BluetoothLeScanner scanner = adapter.getBluetoothLeScanner();
        if (scanner == null) {
            Log.w(TAG, "this device cannot scan");
            return;
        }
        // Without "neverForLocation" (see neededPermissions) Android delivers
        // no scan results while Location is switched off, and says nothing.
        if (!locationEnabled()) {
            Log.w(TAG, "location is off: scans will find nothing");
            main.post(() -> Toast.makeText(
                            context,
                            "Turn on Location to find nearby devices over Bluetooth",
                            Toast.LENGTH_LONG)
                    .show());
        }
        // Screen-off scans are only delivered with a filter.
        ScanFilter filter = new ScanFilter.Builder().setServiceData(SERVICE, new byte[0]).build();
        ScanSettings settings =
                new ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_BALANCED).build();
        scanning =
                new ScanCallback() {
                    @Override
                    public void onScanResult(int callbackType, ScanResult result) {
                        sighted(result);
                    }

                    @Override
                    public void onBatchScanResults(List<ScanResult> results) {
                        for (ScanResult result : results) {
                            sighted(result);
                        }
                    }

                    @Override
                    public void onScanFailed(int errorCode) {
                        Log.w(TAG, "scan failed: " + errorCode);
                    }
                };
        try {
            scanner.startScan(Collections.singletonList(filter), settings, scanning);
        } catch (RuntimeException e) {
            Log.w(TAG, "cannot scan", e);
        }
    }

    private static boolean locationEnabled() {
        LocationManager location = (LocationManager) context.getSystemService(Context.LOCATION_SERVICE);
        return location == null || location.isLocationEnabled();
    }

    private static void sighted(ScanResult result) {
        ScanRecord record = result.getScanRecord();
        byte[] payload = record == null ? null : record.getServiceData(SERVICE);
        if (payload == null || payload.length < TAG_LEN + 2) {
            return;
        }
        byte[] tag = new byte[TAG_LEN];
        System.arraycopy(payload, 0, tag, 0, TAG_LEN);
        int psm = ((payload[TAG_LEN] & 0xff) << 8) | (payload[TAG_LEN + 1] & 0xff);
        String key = hex(tag);
        seen.put(key, new Sighting(result.getDevice(), psm));
        long now = System.currentTimeMillis();
        Long last = reported.get(key);
        if (last == null || now - last >= SEEN_REPORT_MS) {
            reported.put(key, now);
            onPeerSeen(tag);
        }
    }

    /**
     * Queues {@code packet} for the peer {@code tag}, opening a channel to
     * it if there is none. Any thread; never blocks.
     */
    public static void send(byte[] tag, byte[] packet) {
        if (!up) {
            return;
        }
        String key = hex(tag);
        Link link = links.get(key);
        if (link == null) {
            Sighting sighting = seen.get(key);
            Long failed = dialFailed.get(key);
            if (sighting == null
                    || (failed != null && System.currentTimeMillis() - failed < REDIAL_MS)) {
                return;
            }
            link = links.computeIfAbsent(key, k -> Link.dial(k, sighting));
        }
        link.offer(packet);
    }

    private static void acceptLoop(BluetoothServerSocket listening) {
        while (true) {
            BluetoothSocket socket;
            try {
                socket = listening.accept();
            } catch (IOException e) {
                return; // closed by bringDown
            }
            Link.accepted(socket);
        }
    }

    private static final class Sighting {
        final BluetoothDevice device;
        final int psm;

        Sighting(BluetoothDevice device, int psm) {
            this.device = device;
            this.psm = psm;
        }
    }

    /** One L2CAP channel to a peer: a writer and a reader thread. */
    private static final class Link {
        private final ArrayBlockingQueue<byte[]> outgoing = new ArrayBlockingQueue<>(QUEUE);
        private final boolean dialed;
        private volatile BluetoothSocket socket;
        private volatile boolean closed;
        private volatile String key;

        private Link(String key, boolean dialed) {
            this.key = key;
            this.dialed = dialed;
        }

        /** A link to a sighted peer; connects in the background. */
        static Link dial(String key, Sighting sighting) {
            Link link = new Link(key, true);
            new Thread(
                            () -> {
                                try {
                                    BluetoothSocket socket =
                                            sighting.device.createInsecureL2capChannel(
                                                    sighting.psm);
                                    socket.connect();
                                    link.run(socket);
                                } catch (IOException | SecurityException e) {
                                    Log.i(TAG, "dial " + key + " failed: " + e);
                                    dialFailed.put(key, System.currentTimeMillis());
                                    link.close();
                                }
                            },
                            "bt-dial")
                    .start();
            return link;
        }

        /** A channel a peer opened; its first frame names the peer. */
        static void accepted(BluetoothSocket socket) {
            Link link = new Link(null, false);
            new Thread(
                            () -> {
                                try {
                                    link.run(socket);
                                } catch (IOException e) {
                                    link.close();
                                }
                            },
                            "bt-link")
                    .start();
        }

        void offer(byte[] packet) {
            if (!closed) {
                outgoing.offer(packet); // full: drop, like a busy UDP socket
            }
        }

        /** Exchanges tags, then pumps frames both ways until the channel fails. */
        private void run(BluetoothSocket socket) throws IOException {
            this.socket = socket;
            if (closed) {
                closeQuietly(socket);
                return;
            }
            DataOutputStream out =
                    new DataOutputStream(new BufferedOutputStream(socket.getOutputStream()));
            DataInputStream in = new DataInputStream(socket.getInputStream());
            writeFrame(out, localTag);
            out.flush();
            byte[] remote = readFrame(in);
            if (remote.length != TAG_LEN) {
                throw new IOException("bad hello");
            }
            String remoteKey = hex(remote);
            if (dialed && !remoteKey.equals(key)) {
                throw new IOException("dialed " + key + " but reached " + remoteKey);
            }
            if (!dialed) {
                key = remoteKey;
                adopt();
            }
            if (closed) {
                closeQuietly(socket);
                return;
            }
            Thread writer = new Thread(() -> writeLoop(out), "bt-write");
            writer.start();
            try {
                while (!closed) {
                    onPacket(remote, readFrame(in));
                }
            } finally {
                close();
            }
        }

        /**
         * Registers an accepted channel as the way to its peer. When both
         * sides dialed at once, both keep the channel the lower tag opened,
         * so they settle on the same one.
         */
        private void adopt() {
            Link existing = links.get(key);
            if (existing == null || existing == this) {
                links.put(key, this);
                return;
            }
            boolean theirsWins = compare(hexBytes(key), localTag) < 0;
            if (existing.dialed && !theirsWins) {
                close();
            } else {
                links.put(key, this);
                existing.close();
            }
        }

        private void writeLoop(DataOutputStream out) {
            try {
                while (!closed) {
                    byte[] packet = outgoing.poll(1, TimeUnit.SECONDS);
                    if (packet == null) {
                        continue;
                    }
                    writeFrame(out, packet);
                    while ((packet = outgoing.poll()) != null) {
                        writeFrame(out, packet);
                    }
                    out.flush();
                }
            } catch (IOException | InterruptedException e) {
                close();
            }
        }

        void close() {
            closed = true;
            if (key != null) {
                links.remove(key, this);
            }
            closeQuietly(socket);
        }
    }

    private static void writeFrame(DataOutputStream out, byte[] data) throws IOException {
        if (data.length > 0xffff) {
            return; // never happens for QUIC packets; dropping keeps framing intact
        }
        out.writeShort(data.length);
        out.write(data);
    }

    private static byte[] readFrame(DataInputStream in) throws IOException {
        int len = in.readUnsignedShort();
        byte[] data = new byte[len];
        in.readFully(data);
        return data;
    }

    private static void closeQuietly(java.io.Closeable closeable) {
        if (closeable == null) {
            return;
        }
        try {
            closeable.close();
        } catch (IOException e) {
            // closing anyway
        }
    }

    private static String hex(byte[] bytes) {
        StringBuilder sb = new StringBuilder(bytes.length * 2);
        for (byte b : bytes) {
            sb.append(String.format("%02x", b & 0xff));
        }
        return sb.toString();
    }

    private static byte[] hexBytes(String hex) {
        byte[] bytes = new byte[hex.length() / 2];
        for (int i = 0; i < bytes.length; i++) {
            bytes[i] = (byte) Integer.parseInt(hex.substring(2 * i, 2 * i + 2), 16);
        }
        return bytes;
    }

    private static int compare(byte[] a, byte[] b) {
        for (int i = 0; i < Math.min(a.length, b.length); i++) {
            int d = (a[i] & 0xff) - (b[i] & 0xff);
            if (d != 0) {
                return d;
            }
        }
        return a.length - b.length;
    }
}
