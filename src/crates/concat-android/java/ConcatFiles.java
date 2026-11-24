// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

package app.concat.editor;

import android.Manifest;
import android.app.Activity;
import android.app.ActivityManager;
import android.app.ApplicationExitInfo;
import android.app.Fragment;
import android.app.FragmentManager;
import android.content.ClipData;
import android.content.ContentResolver;
import android.content.ContentUris;
import android.content.ContentValues;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.res.AssetFileDescriptor;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.CancellationSignal;
import android.os.Environment;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.util.Log;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicReference;

/**
 * The system's document picker, for an activity that has no Java of its
 * own to receive its answer.
 *
 * The window is a NativeActivity: the framework's own class, with nothing
 * of ours to override, and a picker's result comes back only through an
 * activity's or a fragment's onActivityResult. So this is a fragment - a
 * headless one, added to the activity for the length of one pick - and
 * the result comes back here.
 *
 * What is picked is a content URI, which the engine's decoder cannot open:
 * it reads files by path. Each is copied into the app's own external
 * files, under Imported/, keeping its display name, and the paths are
 * what the engine is handed - through the native method below, which the
 * Rust side registers before it asks for a pick.
 *
 * The way out is here as well: a finished export is written into the
 * app's own folder, which on a phone nothing but the app can open, so
 * {@link #publishVideo} moves it into the phone's Movies through the
 * media store, where the gallery and every file manager show it.
 *
 * And the microphone, for a voiceover: a permission granted while the
 * app runs, asked for through the activity by {@link #askMicrophone}.
 *
 * Compiled by build.rs with the SDK's javac and d8, and loaded at run time
 * from the dex inside the binary; see src/lib.rs.
 */
public class ConcatFiles extends Fragment {
    private static final int REQUEST_PICK = 0xC0C4;
    private static final int REQUEST_MICROPHONE = 0xC0C5;
    private static final String TAG = "concat-files";

    /**
     * The pick the native side waits for. A copy still running for an
     * older pick must not deliver into the new one's callback, so only
     * this fragment's answer is sent.
     */
    private static final AtomicReference<ConcatFiles> current = new AtomicReference<ConcatFiles>();
    private long requestId;
    private final OwnedCopies copies = new OwnedCopies();
    private volatile boolean cancelled;
    private volatile InputStream activeInput;
    private volatile CancellationSignal activeCancellation;
    private long nextProgress;


    /**
     * Where a finished export goes, under the phone's storage: Movies/Concat.
     * Null before Android 10, which has no media store an app can write
     * through without a permission; there the file stays in the app's folder.
     */
    public static String publishedFolder() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            return null;
        }
        return Environment.DIRECTORY_MOVIES + "/Concat";
    }

    /**
     * Moves the video at {@code path} into {@link #publishedFolder()} through
     * the media store and deletes the original. Two strings come back: the
     * file's path under the phone's storage and null, or null and why it
     * could not be moved - in which case the original is still where it was.
     * Blocking: a video is big and the copy takes a while, so this is called
     * off the window's thread.
     */
    public static String[] publishVideo(Activity activity, String path) {
        String folder = publishedFolder();
        if (folder == null) {
            return new String[] {null, "this phone has no media store to put it in"};
        }
        return publish(activity, path, "video/mp4", folder,
                MediaStore.Video.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY));
    }

    /**
     * {@link #publishVideo} for a small file that is not a video - a
     * subtitle file - of MIME type {@code mime}: moved into the phone's
     * Download/Concat, which every file manager shows, under its own name
     * or "name (1)" where that is taken. The same two strings come back.
     */
    public static String[] publishDocument(Activity activity, String path, String mime) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            return new String[] {null, "this phone has no media store to put it in"};
        }
        return publish(activity, path, mime, Environment.DIRECTORY_DOWNLOADS + "/Concat",
                MediaStore.Downloads.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY));
    }

    /** Moves the file at {@code path} into {@code collection}, under {@code folder}. */
    private static String[] publish(
            Activity activity, String path, String mime, String folder, Uri collection) {
        File file = new File(path);
        ContentResolver resolver = activity.getContentResolver();
        ContentValues values = new ContentValues();
        values.put(MediaStore.MediaColumns.DISPLAY_NAME, file.getName());
        values.put(MediaStore.MediaColumns.MIME_TYPE, mime);
        values.put(MediaStore.MediaColumns.RELATIVE_PATH, folder);
        values.put(MediaStore.MediaColumns.IS_PENDING, 1);
        Uri uri;
        try {
            uri = resolver.insert(collection, values);
        } catch (Exception e) {
            return new String[] {null, "the media store refused the file: " + e.getMessage()};
        }
        if (uri == null) {
            return new String[] {null, "the media store refused the file"};
        }
        InputStream in = null;
        OutputStream out = null;
        try {
            in = new FileInputStream(file);
            out = resolver.openOutputStream(uri);
            if (out == null) {
                throw new IOException("the media store would not open the file for writing");
            }
            byte[] buffer = new byte[1 << 16];
            int read;
            while ((read = in.read(buffer)) > 0) {
                out.write(buffer, 0, read);
            }
            out.close();
            out = null;
            values.clear();
            values.put(MediaStore.MediaColumns.IS_PENDING, 0);
            resolver.update(uri, values, null, null);
        } catch (Exception e) {
            try {
                resolver.delete(uri, null, null);
            } catch (Exception ignored) {
            }
            return new String[] {null, e.getMessage() == null ? e.toString() : e.getMessage()};
        } finally {
            try {
                if (in != null) in.close();
            } catch (IOException ignored) {
            }
            try {
                if (out != null) out.close();
            } catch (IOException ignored) {
            }
        }
        file.delete();
        // The store may have renamed it: a second "name.mp4" is "name (1).mp4".
        return new String[] {folder + "/" + displayName(activity, uri), null};
    }

    /**
     * Whether the app may record from the microphone. When it may not yet,
     * the system's question is put up and false comes back at once; the
     * answer is read the next time this is asked, which is the voiceover
     * panel's Try again. From any thread.
     */
    public static boolean askMicrophone(final Activity activity) {
        if (Build.VERSION.SDK_INT < 23) {
            return true;
        }
        if (activity.checkSelfPermission(Manifest.permission.RECORD_AUDIO)
                == PackageManager.PERMISSION_GRANTED) {
            return true;
        }
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                activity.requestPermissions(
                        new String[] {Manifest.permission.RECORD_AUDIO}, REQUEST_MICROPHONE);
            }
        });
        return false;
    }

    /**
     * Keeps the screen on while a job runs, or lets it go dark again (#695).
     * From any thread, and without waiting: the window's flag is set on the
     * activity's own thread. The native side must not set it through
     * android-activity, whose lock the window's thread holds while it
     * handles the event that starts the job.
     */
    public static void keepScreenOn(final Activity activity, final boolean on) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (on) {
                    activity.getWindow().addFlags(
                            android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                } else {
                    activity.getWindow().clearFlags(
                            android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                }
            }
        });
    }

    /**
     * Opens a web address in the browser, or in whichever app claims it -
     * the launch screen's guides, the repository, the Discord invite. Null
     * when it went, or why it could not. Asked straight rather than through
     * resolveActivity, which from Android 11 answers no for any app the
     * manifest does not name in its queries. From any thread.
     */
    public static String openLink(Activity activity, String url) {
        try {
            Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse(url));
            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            activity.startActivity(intent);
            return null;
        } catch (android.content.ActivityNotFoundException e) {
            return "no app on this phone opens " + url;
        } catch (RuntimeException e) {
            Log.w(TAG, "could not open " + url, e);
            return String.valueOf(e.getMessage());
        }
    }

    /**
     * Hands the log files at {@code paths} - one per line - to the system's
     * share sheet, so a report can carry them to mail, a chat, a drive.
     * Nothing else on the phone can read the app's own folder, so from
     * Android 10 each is copied into Download/Concat as a text file, the
     * same run's copy written over, and shared by the media store's URI with
     * a read grant: no permission asked, no file manager needed. Before
     * Android 10 an app cannot write there without a permission, and the
     * end of the logs goes as text instead. Null when the sheet went up, or
     * why it could not. From any thread.
     */
    public static String shareLogs(Activity activity, String paths) {
        try {
            String[] files = paths.split("\n");
            ArrayList<Uri> uris = new ArrayList<>();
            Intent send = new Intent(files.length > 1 && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q
                    ? Intent.ACTION_SEND_MULTIPLE : Intent.ACTION_SEND);
            send.setType("text/plain");
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                for (String path : files) {
                    uris.add(logCopy(activity, new File(path)));
                }
                ClipData clip = ClipData.newRawUri(null, uris.get(0));
                for (int i = 1; i < uris.size(); i++) {
                    clip.addItem(new ClipData.Item(uris.get(i)));
                }
                send.setClipData(clip);
                send.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                if (uris.size() > 1) {
                    send.putParcelableArrayListExtra(Intent.EXTRA_STREAM, uris);
                } else {
                    send.putExtra(Intent.EXTRA_STREAM, uris.get(0));
                }
            } else {
                // A share's extras cross to the other app in one binder
                // call, which holds about a megabyte: each file's end.
                StringBuilder text = new StringBuilder();
                for (String path : files) {
                    File file = new File(path);
                    text.append("==== ").append(file.getName()).append(" ====\n");
                    text.append(tail(file, 120 * 1024)).append('\n');
                }
                send.putExtra(Intent.EXTRA_TEXT, text.toString());
            }
            activity.startActivity(Intent.createChooser(send, null));
            return null;
        } catch (Exception e) {
            Log.w(TAG, "could not share the log", e);
            return e.getMessage() == null ? e.toString() : e.getMessage();
        }
    }

    /** The log at {@code file}, copied into Download/Concat as text. */
    private static Uri logCopy(Activity activity, File file) throws IOException {
        ContentResolver resolver = activity.getContentResolver();
        Uri collection = MediaStore.Downloads.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY);
        String folder = Environment.DIRECTORY_DOWNLOADS + "/Concat/";
        String name = file.getName().replaceFirst("\\.log$", "") + ".txt";
        Uri uri = null;
        // The copy an earlier share of this run made, which this one
        // replaces rather than adding "name (1)" beside it.
        Cursor cursor = resolver.query(collection, new String[] {MediaStore.MediaColumns._ID},
                MediaStore.MediaColumns.DISPLAY_NAME + "=? AND "
                        + MediaStore.MediaColumns.RELATIVE_PATH + "=?",
                new String[] {name, folder}, null);
        if (cursor != null) {
            if (cursor.moveToFirst()) {
                uri = ContentUris.withAppendedId(collection, cursor.getLong(0));
            }
            cursor.close();
        }
        if (uri == null) {
            ContentValues values = new ContentValues();
            values.put(MediaStore.MediaColumns.DISPLAY_NAME, name);
            values.put(MediaStore.MediaColumns.MIME_TYPE, "text/plain");
            values.put(MediaStore.MediaColumns.RELATIVE_PATH, folder);
            uri = resolver.insert(collection, values);
            if (uri == null) {
                throw new IOException("the media store refused " + name);
            }
        }
        InputStream in = new FileInputStream(file);
        try {
            OutputStream out = resolver.openOutputStream(uri, "wt");
            if (out == null) {
                throw new IOException("the media store would not open " + name + " for writing");
            }
            try {
                byte[] buffer = new byte[1 << 16];
                int read;
                while ((read = in.read(buffer)) > 0) {
                    out.write(buffer, 0, read);
                }
            } finally {
                out.close();
            }
        } finally {
            in.close();
        }
        return uri;
    }

    /** The last {@code most} bytes of {@code file}, as text. */
    private static String tail(File file, int most) throws IOException {
        java.io.RandomAccessFile in = new java.io.RandomAccessFile(file, "r");
        try {
            long from = Math.max(0, in.length() - most);
            byte[] bytes = new byte[(int) (in.length() - from)];
            in.seek(from);
            in.readFully(bytes);
            return new String(bytes, StandardCharsets.UTF_8);
        } finally {
            in.close();
        }
    }

    /**
     * How the system says the app's last process ended, read once as the
     * app starts; Android 11 and on keep a record, and null comes back on
     * an older phone or when there is none. Four strings: the log file
     * that process was writing (as {@code log} named it for this one), or
     * empty; "died" when it ended in a way nobody asked for - a crash, a
     * hang, killed while on screen - or "ended"; what the system says, for
     * the log; and, for a hang (ANR), where the threads that matter were
     * when the system gave up on it, or empty. A native crash brings its
     * signal, thread, abort message and top frames, read from the crash
     * report the system kept.
     *
     * This run is then marked with {@code log}, so the next run knows which
     * file this one is.
     */
    public static String[] lastExit(Activity activity, String log) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
            return null;
        }
        ActivityManager manager =
                (ActivityManager) activity.getSystemService(Context.ACTIVITY_SERVICE);
        String[] answer = null;
        try {
            List<ApplicationExitInfo> exits =
                    manager.getHistoricalProcessExitReasons(null, 0, 1);
            if (!exits.isEmpty()) {
                answer = describe(exits.get(0));
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "could not read how the last run ended", e);
        }
        try {
            manager.setProcessStateSummary(log.getBytes(StandardCharsets.UTF_8));
        } catch (RuntimeException e) {
            Log.w(TAG, "could not mark this run", e);
        }
        return answer;
    }

    private static String[] describe(ApplicationExitInfo exit) {
        int status = exit.getStatus();
        boolean seen = exit.getImportance()
                <= ActivityManager.RunningAppProcessInfo.IMPORTANCE_VISIBLE;
        String reason;
        boolean died;
        switch (exit.getReason()) {
            case ApplicationExitInfo.REASON_CRASH_NATIVE:
                reason = "a native crash, signal " + status;
                died = true;
                break;
            case ApplicationExitInfo.REASON_CRASH:
                reason = "an uncaught Java exception";
                died = true;
                break;
            case ApplicationExitInfo.REASON_ANR:
                reason = "not responding (ANR)";
                died = true;
                break;
            case ApplicationExitInfo.REASON_INITIALIZATION_FAILURE:
                reason = "a failure to start";
                died = true;
                break;
            case ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE:
                reason = "killed for excessive resource use";
                died = true;
                break;
            case ApplicationExitInfo.REASON_LOW_MEMORY:
                reason = "killed for memory";
                died = seen;
                break;
            case ApplicationExitInfo.REASON_SIGNALED:
                reason = "killed by signal " + status;
                died = seen;
                break;
            case ApplicationExitInfo.REASON_EXIT_SELF:
                reason = "its own exit, status " + status;
                died = status != 0;
                break;
            case ApplicationExitInfo.REASON_USER_REQUESTED:
                reason = "the person (force stop, or swiped away)";
                died = false;
                break;
            default:
                reason = "reason " + exit.getReason() + ", status " + status;
                died = false;
        }
        StringBuilder detail = new StringBuilder(reason);
        detail.append(seen ? " · on screen" : " · in the background");
        if (exit.getPss() > 0) {
            detail.append(" · ").append(exit.getPss() / 1024).append(" MB in use");
        }
        if (exit.getDescription() != null) {
            detail.append(" · \"").append(exit.getDescription()).append('"');
        }
        if (exit.getReason() == ApplicationExitInfo.REASON_CRASH_NATIVE
                && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            try {
                InputStream trace = exit.getTraceInputStream();
                if (trace != null) {
                    detail.append(" · ").append(Tombstone.summary(readAll(trace)));
                }
            } catch (IOException | RuntimeException e) {
                detail.append(" · its crash report could not be read: ").append(e);
            }
        }
        // A hang's record carries the system's dump of every thread as it
        // was when the app was given up on: the one place that says what
        // the window was doing instead of answering.
        String threads = "";
        if (exit.getReason() == ApplicationExitInfo.REASON_ANR) {
            try {
                InputStream trace = exit.getTraceInputStream();
                if (trace != null) {
                    threads = AnrTrace.threads(new String(readAll(trace), StandardCharsets.UTF_8));
                } else {
                    threads = "the system kept no dump of its threads";
                }
            } catch (IOException | RuntimeException e) {
                threads = "its threads' dump could not be read: " + e;
            }
        }
        byte[] mark = exit.getProcessStateSummary();
        String log = mark == null ? "" : new String(mark, StandardCharsets.UTF_8);
        return new String[] {log, died ? "died" : "ended", detail.toString(), threads};
    }

    /**
     * The threads of an ANR dump worth a log: the Java main thread, and
     * every thread standing in Concat's own library - the window's thread
     * among them, which on a NativeActivity is not "main". The dump is the
     * runtime's text or the system's native one, one block a thread, each
     * opening with the thread's quoted name, with a blank line after; each
     * block kept is its first line and its frames, at most
     * {@link #FRAMES} of them, and all of them together at most
     * {@link #BUDGET} characters, the main thread first.
     */
    static final class AnrTrace {
        /** The library cargo-apk packages Concat's code as. */
        static final String LIBRARY = "libconcat_android.so";
        static final int FRAMES = 40;
        static final int BUDGET = 12 * 1024;

        static String threads(String dump) {
            List<String> main = new ArrayList<>();
            List<String> ours = new ArrayList<>();
            // A thread's block opens with its quoted name at the start of
            // a line - straight under "DALVIK THREADS (n):" for the first -
            // and ends at a blank line.
            // The process's own id, from the "----- pid N at ... -----" a
            // dump opens with: a native dump names the main thread after
            // the process, and only its sysTid, the process's id, says
            // which it is.
            String pid = "";
            StringBuilder block = null;
            for (String line : (dump + "\n").split("\\r?\\n")) {
                if (line.startsWith("----- pid ")) {
                    String[] words = line.split(" ");
                    pid = words.length > 2 ? words[2] : "";
                }
                if (line.startsWith("\"")) {
                    sort(block, pid, main, ours);
                    block = new StringBuilder();
                } else if (line.trim().isEmpty()) {
                    sort(block, pid, main, ours);
                    block = null;
                }
                if (block != null) {
                    block.append(line).append('\n');
                }
            }
            sort(block, pid, main, ours);
            List<String> blocks = new ArrayList<>(main);
            blocks.addAll(ours);
            if (blocks.isEmpty()) {
                return "its threads' dump named neither the main thread nor " + LIBRARY;
            }
            StringBuilder out = new StringBuilder();
            int left = blocks.size();
            for (String thread : blocks) {
                if (out.length() + thread.length() > BUDGET) {
                    out.append(left).append(" more thread(s) left out for length\n");
                    break;
                }
                out.append(thread);
                left--;
            }
            return out.toString().trim();
        }

        /**
         * A finished block to the main thread's place, Concat's, or none.
         * The runtime's dump names the main thread "main"; the system's
         * native one, which it falls back on when the runtime does not
         * answer, names it after the process, with the process's id.
         */
        private static void sort(
                StringBuilder block, String pid, List<String> main, List<String> ours) {
            if (block == null) {
                return;
            }
            String text = block.toString().trim();
            String name = text.split("\\r?\\n", 2)[0];
            if (name.startsWith("\"main\"")
                    || (!pid.isEmpty() && name.endsWith(" sysTid=" + pid))) {
                main.add(kept(text));
            } else if (name.startsWith("\"android_main\"")) {
                // The window's own thread, the one a hang is most likely
                // in: first of Concat's, whatever the budget leaves out.
                ours.add(0, kept(text));
            } else if (text.contains(LIBRARY)) {
                ours.add(kept(text));
            }
        }

        /** One thread's block: its name line, then its frames. */
        private static String kept(String block) {
            String[] lines = block.split("\\r?\\n");
            StringBuilder out = new StringBuilder(lines[0].trim()).append('\n');
            int frames = 0;
            int cut = 0;
            for (int i = 1; i < lines.length; i++) {
                String line = lines[i].trim();
                // The runtime's own facts about the thread - its group, its
                // scheduler, its stack's bounds, how long it took to dump -
                // are left; the state is in the name line.
                if (line.isEmpty() || line.startsWith("|") || line.startsWith("DumpLatencyMs")) {
                    continue;
                }
                if (frames < FRAMES) {
                    // A file by its name alone: the app's folder under
                    // /data/app is two random names long, and the rest of
                    // the path says nothing the name does not.
                    out.append("  ").append(line.replaceAll("(?<=\\s)/\\S*/", "")).append('\n');
                    frames++;
                } else {
                    cut++;
                }
            }
            if (cut > 0) {
                out.append("  ... ").append(cut).append(" more frame(s)\n");
            }
            return out.toString();
        }
    }

    private static byte[] readAll(InputStream in) throws IOException {
        try {
            java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
            byte[] buffer = new byte[1 << 16];
            int read;
            while ((read = in.read(buffer)) > 0) {
                out.write(buffer, 0, read);
            }
            return out.toByteArray();
        } finally {
            in.close();
        }
    }

    /**
     * The few fields of a native crash report worth a log line, read from
     * the system's protobuf (debuggerd's tombstone.proto) by hand: the
     * signal, the thread it hit, the abort message and the top frames.
     */
    static final class Tombstone {
        private final byte[] data;
        private int at;
        private final int end;

        private Tombstone(byte[] data, int from, int end) {
            this.data = data;
            this.at = from;
            this.end = end;
        }

        static String summary(byte[] data) {
            Tombstone top = new Tombstone(data, 0, data.length);
            long tid = -1;
            String signal = "", abort = "", cause = "";
            java.util.Map<Long, int[]> threads = new java.util.HashMap<>();
            while (top.more()) {
                long key = top.varint();
                int field = (int) (key >>> 3);
                if (field == 6 && (key & 7) == 0) {
                    tid = top.varint();
                } else if (field == 10 && (key & 7) == 2) {
                    Tombstone info = top.message();
                    String name = "", code = "";
                    while (info.more()) {
                        long k = info.varint();
                        if (k >>> 3 == 2 && (k & 7) == 2) name = info.string();
                        else if (k >>> 3 == 4 && (k & 7) == 2) code = info.string();
                        else info.skip(k);
                    }
                    signal = name + (code.isEmpty() ? "" : " (" + code + ")");
                } else if (field == 14 && (key & 7) == 2) {
                    abort = top.string();
                } else if (field == 15 && (key & 7) == 2 && cause.isEmpty()) {
                    Tombstone c = top.message();
                    while (c.more()) {
                        long k = c.varint();
                        if (k >>> 3 == 1 && (k & 7) == 2) cause = c.string();
                        else c.skip(k);
                    }
                } else if (field == 16 && (key & 7) == 2) {
                    // A map entry: the thread's id, and where the thread is.
                    Tombstone entry = top.message();
                    long id = -1;
                    int[] span = null;
                    while (entry.more()) {
                        long k = entry.varint();
                        if (k >>> 3 == 1 && (k & 7) == 0) {
                            id = entry.varint();
                        } else if (k >>> 3 == 2 && (k & 7) == 2) {
                            int length = (int) entry.varint();
                            span = new int[] {entry.at, entry.at + length};
                            entry.at += length;
                        } else {
                            entry.skip(k);
                        }
                    }
                    if (span != null) threads.put(id, span);
                } else {
                    top.skip(key);
                }
            }
            StringBuilder out = new StringBuilder("crash report: ").append(signal);
            int[] span = threads.get(tid);
            if (span != null) {
                Tombstone thread = new Tombstone(data, span[0], span[1]);
                String name = "";
                StringBuilder frames = new StringBuilder();
                int count = 0;
                while (thread.more()) {
                    long k = thread.varint();
                    if (k >>> 3 == 2 && (k & 7) == 2) {
                        name = thread.string();
                    } else if (k >>> 3 == 4 && (k & 7) == 2 && count < 12) {
                        frames.append(" | ").append(frame(thread.message()));
                        count++;
                    } else {
                        thread.skip(k);
                    }
                }
                out.append(" in thread ").append(tid).append(" (").append(name).append(')');
                if (!cause.isEmpty()) out.append(" · cause: ").append(cause);
                if (!abort.isEmpty()) out.append(" · abort message: \"").append(abort).append('"');
                out.append(" · frames").append(frames);
            } else {
                if (!cause.isEmpty()) out.append(" · cause: ").append(cause);
                if (!abort.isEmpty()) out.append(" · abort message: \"").append(abort).append('"');
            }
            return out.toString();
        }

        /** One frame: `libconcat_android.so+0x1a2b3c (function+12)`. */
        private static String frame(Tombstone frame) {
            long pc = 0, offset = 0;
            String file = "", function = "";
            while (frame.more()) {
                long k = frame.varint();
                int field = (int) (k >>> 3);
                if (field == 1 && (k & 7) == 0) pc = frame.varint();
                else if (field == 4 && (k & 7) == 2) function = frame.string();
                else if (field == 5 && (k & 7) == 0) offset = frame.varint();
                else if (field == 6 && (k & 7) == 2) file = frame.string();
                else frame.skip(k);
            }
            String module = file.substring(file.lastIndexOf('/') + 1);
            String where = module + "+0x" + Long.toHexString(pc);
            return function.isEmpty() ? where : where + " (" + function + "+" + offset + ")";
        }

        private boolean more() {
            return at < end;
        }

        private long varint() {
            long value = 0;
            for (int shift = 0; shift < 64 && at < end; shift += 7) {
                byte b = data[at++];
                value |= (long) (b & 0x7f) << shift;
                if ((b & 0x80) == 0) {
                    break;
                }
            }
            return value;
        }

        private Tombstone message() {
            int length = (int) varint();
            Tombstone inner = new Tombstone(data, at, Math.min(end, at + length));
            at = Math.min(end, at + length);
            return inner;
        }

        private String string() {
            Tombstone inner = message();
            return new String(data, inner.at, inner.end - inner.at, StandardCharsets.UTF_8);
        }

        /** Steps over a field this does not read. */
        private void skip(long key) {
            switch ((int) (key & 7)) {
                case 0: varint(); break;
                case 1: at += 8; break;
                case 2: message(); break;
                case 5: at += 4; break;
                default: at = end;
            }
        }
    }

    /** Registered from Rust: the picked files' paths, or none. */
    public static native void filesPicked(long request, String[] paths, String[] failedNames, String[] failures);

    /** Returns false for a cancelled, replaced or already delivered request. */
    public static native boolean copyProgress(long request, int completed, int total,
            long copiedBytes, long totalBytes);

    /** A native UI decision for this exact request, never the next picker. */
    public static boolean finishPick(long request, boolean accepted) {
        ConcatFiles fragment = current.get();
        if (fragment != null && fragment.requestId == request) return fragment.settle(accepted);
        // Retirement is idempotent. Acceptance always needs the exact live
        // owner to acknowledge it before native code receives any paths.
        return !accepted;
    }

    /**
     * Shows the picker, offering the MIME types in {@code types} - comma
     * separated, "video/*,audio/*" - or every file when it is empty. From
     * any thread; the fragment is added on the UI thread.
     */
    public static void pick(final Activity activity, final String types, final long request) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                // A queued old launch must not replace a newer native request.
                if (!copyProgress(request, 0, 0, 0, 0)) return;
                FragmentManager manager = activity.getFragmentManager();
                Fragment previous = manager.findFragmentByTag(TAG);
                if (previous != null) {
                    // A pick already up is replaced, not stacked: its late
                    // answer must not reach the new pick's callback.
                    manager.beginTransaction().remove(previous).commitAllowingStateLoss();
                    manager.executePendingTransactions();
                }
                ConcatFiles fragment = new ConcatFiles();
                fragment.requestId = request;
                ConcatFiles retired = current.getAndSet(fragment);
                if (retired != null) retired.settle(false);
                manager.beginTransaction()
                        .add(fragment, TAG)
                        .commitAllowingStateLoss();
                manager.executePendingTransactions();

                Intent photos = photoPicker(types);
                if (photos != null) {
                    try {
                        fragment.startActivityForResult(photos, REQUEST_PICK);
                        return;
                    } catch (Exception unavailable) {
                        Log.i(TAG, "system photo picker unavailable; opening Files", unavailable);
                    }
                }
                try {
                    fragment.startActivityForResult(documentPicker(types), REQUEST_PICK);
                } catch (Exception error) {
                    fragment.answer(new String[0], new String[] {""},
                            new String[] {reason(error)});
                }
            }
        });
    }

    /** Native visual picker; Files and audio always retain the document path. */
    private static Intent photoPicker(String types) {
        if (types == null || types.isEmpty() || types.contains("audio/")) return null;
        boolean available = Build.VERSION.SDK_INT >= 33
                || (Build.VERSION.SDK_INT >= 30
                    && android.os.ext.SdkExtensions.getExtensionVersion(Build.VERSION_CODES.R) >= 2);
        if (!available) return null;
        Intent intent = new Intent(MediaStore.ACTION_PICK_IMAGES);
        if (types.equals("image/*") || types.equals("video/*")) intent.setType(types);
        intent.putExtra(MediaStore.EXTRA_PICK_IMAGES_MAX, MediaStore.getPickImagesMaxLimit());
        // This optional extra is ignored by older picker implementations.
        intent.putExtra("android.provider.extra.PICK_IMAGES_IN_ORDER", true);
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        return intent;
    }

    private static Intent documentPicker(String types) {
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        intent.setType("*/*");
        if (types != null && !types.isEmpty()) {
            intent.putExtra(Intent.EXTRA_MIME_TYPES, types.split(","));
        }
        intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        return intent;
    }

    private static String reason(Exception error) {
        String message = error.getMessage();
        return message == null || message.isEmpty() ? error.toString() : message;
    }

    @Override
    public void onActivityResult(int request, int result, Intent data) {
        if (request != REQUEST_PICK) {
            return;
        }
        List<Uri> uris = new ArrayList<Uri>();
        if (result == Activity.RESULT_OK && data != null) {
            ClipData clip = data.getClipData();
            if (clip != null) {
                for (int i = 0; i < clip.getItemCount(); i++) {
                    Uri uri = clip.getItemAt(i).getUri();
                    if (uri != null) {
                        uris.add(uri);
                    }
                }
            } else if (data.getData() != null) {
                uris.add(data.getData());
            }
        }
        done(uris);
    }

    /** Copies what was picked and reports it; native UI still owns acceptance. */
    private void done(final List<Uri> uris) {
        final Activity activity = getActivity();
        try {
            getFragmentManager().beginTransaction().remove(this).commitAllowingStateLoss();
        } catch (Exception ignored) {
        }
        if (activity == null || uris.isEmpty()) {
            answer(new String[0]);
            return;
        }
        new Thread(new Runnable() {
            @Override
            public void run() {
                List<String> paths = new ArrayList<String>();
                List<String> names = new ArrayList<String>();
                List<String> failures = new ArrayList<String>();
                try {
                    report(0, uris.size(), 0, 0, true);
                    File base = activity.getExternalFilesDir(null);
                    if (base == null) base = activity.getFilesDir();
                    File imported = new File(base, "Imported");
                    checkAllowed();
                    if (!imported.isDirectory() && !imported.mkdirs()) {
                        throw new IOException("cannot create persistent media directory: " + imported);
                    }
                    // No later request reuses this directory, including after
                    // a failed file. Retirement cannot delete another pick.
                    File dir = new File(imported, "pick-" + UUID.randomUUID().toString());
                    checkAllowed();
                    copies.directory(dir);
                    for (int index = 0; index < uris.size(); index++) {
                        report(index, uris.size(), 0, 0, true);
                        Uri uri = uris.get(index);
                        String name = uri.getLastPathSegment();
                        if (name == null) name = "";
                        CancellationSignal signal = new CancellationSignal();
                        activeCancellation = signal;
                        try {
                            checkAllowed();
                            name = displayName(activity, uri, signal);
                            File out = copies.reserve(name);
                            copy(activity, uri, out, index, uris.size(), signal);
                            paths.add(out.getAbsolutePath());
                        } catch (CopyCancelled stopped) {
                            throw stopped;
                        } catch (IOException | RuntimeException error) {
                            checkAllowed();
                            names.add(name);
                            failures.add(reason(error));
                        } finally {
                            activeCancellation = null;
                        }
                        report(index + 1, uris.size(), 0, 0, true);
                    }
                    checkAllowed();
                    answer(paths.toArray(new String[0]), names.toArray(new String[0]), failures.toArray(new String[0]));
                } catch (CopyCancelled stopped) {
                    settle(false);
                    answer(new String[0]);
                } catch (IOException | RuntimeException error) {
                    names.add(""); failures.add(reason(error));
                    answer(paths.toArray(new String[0]), names.toArray(new String[0]), failures.toArray(new String[0]));
                } finally {
                    if (cancelled) cleanupCopies();
                }
            }
        }, TAG).start();
    }

    private void checkAllowed() throws CopyCancelled {
        if (cancelled || current.get() != this || !copyProgress(requestId, 0, 0, 0, 0)) {
            throw new CopyCancelled();
        }
    }

    private void report(int done, int total, long bytes, long size, boolean force) throws CopyCancelled {
        checkAllowed();
        long now = System.nanoTime();
        if (force || now >= nextProgress) {
            nextProgress = now + 100000000L;
            if (!copyProgress(requestId, done, total, bytes, size)) throw new CopyCancelled();
        }
    }

    /** Stop I/O immediately; slow provider close and disk cleanup are workers. */
    private boolean settle(boolean keep) {
        if (keep) {
            if (!copies.accept()) return false;
            current.compareAndSet(this, null);
            return true;
        }
        // Only the first cancellation starts an interrupt worker. A later
        // cancellation cannot remove files already accepted by native code.
        if (!copies.cancel()) return true;
        cancelled = true;
        final InputStream interrupt = activeInput;
        final CancellationSignal signal = activeCancellation;
        current.compareAndSet(this, null);
        new Thread(new Runnable() {
            @Override public void run() {
                try {
                    if (signal != null) signal.cancel();
                } catch (RuntimeException error) {
                    Log.w(TAG, "provider did not acknowledge cancellation", error);
                }
                try {
                    if (interrupt != null) interrupt.close();
                } catch (IOException | RuntimeException error) {
                    Log.w(TAG, "provider stream did not close cleanly", error);
                } finally {
                    cleanupCopies();
                }
            }
        }, TAG + "-cancel").start();
        return true;
    }

    /** Only files exclusively reserved by this request may be removed. */
    private void cleanupCopies() {
        for (File file : copies.cleanup()) {
            Log.w(TAG, "could not remove retired selection copy " + file);
        }
    }

    /** File ownership has no Activity dependency and is exercised directly. */
    static final class OwnedCopies {
        private final List<File> files = new ArrayList<File>();
        private File directory;
        private boolean accepted;
        private boolean cancelled;

        synchronized void directory(File dir) throws IOException {
            if (cancelled || accepted) throw new CopyCancelled();
            if (directory != null || !dir.mkdir()) throw new IOException("cannot reserve persistent selection directory");
            directory = dir;
        }

        synchronized File reserve(String name) throws IOException {
            if (cancelled || accepted) throw new CopyCancelled();
            if (directory == null) throw new IOException("selection directory was not reserved");
            File out = unique(directory, name);
            files.add(out);
            return out;
        }

        synchronized boolean accept() {
            if (cancelled) return false;
            accepted = true;
            // Empty failed selections do not leave an empty directory. This
            // never removes a directory containing a completed accepted file.
            if (directory != null) directory.delete();
            files.clear();
            directory = null;
            return true;
        }

        synchronized boolean cancel() {
            if (cancelled || accepted) return false;
            cancelled = true;
            return true;
        }

        synchronized List<File> cleanup() {
            List<File> failed = new ArrayList<File>();
            if (accepted || !cancelled) return failed;
            for (File file : files) {
                if (!file.delete() && file.exists()) failed.add(file);
            }
            // Refuses non-empty directories; unrelated content is preserved.
            if (directory != null && !directory.delete() && directory.exists()) failed.add(directory);
            return failed;
        }
    }

    private void answer(String[] paths) {
        answer(paths, new String[0], new String[0]);
    }

    private void answer(String[] paths, String[] names, String[] failures) {
        if (!cancelled && current.get() == this) {
            filesPicked(requestId, paths, names, failures);
        } else {
            settle(false);
        }
    }

    /** The name the picker showed for the file, or one made from the URI. */
    private static String displayName(Activity activity, Uri uri) {
        return displayName(activity, uri, null);
    }

    private static String displayName(Activity activity, Uri uri, CancellationSignal signal) {
        String name = null;
        Cursor cursor = null;
        try {
            cursor = activity.getContentResolver().query(uri, null, null, null, null, signal);
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (column >= 0) {
                    name = cursor.getString(column);
                }
            }
        } catch (Exception error) {
            if (signal != null && signal.isCanceled()) throw new android.os.OperationCanceledException();
        } finally {
            if (cursor != null) {
                cursor.close();
            }
        }
        if (name == null || name.isEmpty()) {
            name = uri.getLastPathSegment();
        }
        if (name == null || name.isEmpty()) {
            name = "import";
        }
        // Whatever the provider called it, it is one file name.
        return name.replace('/', '_').replace('\\', '_');
    }

    /** `name`, or `name (2)`, `name (3)`... until nothing is overwritten. */
    private static synchronized File unique(File dir, String name) throws IOException {
        File out = new File(dir, name);
        if (out.createNewFile()) {
            return out;
        }
        int dot = name.lastIndexOf('.');
        String stem = dot > 0 ? name.substring(0, dot) : name;
        String ext = dot > 0 ? name.substring(dot) : "";
        for (int n = 2; ; n++) {
            out = new File(dir, stem + " (" + n + ")" + ext);
            if (out.createNewFile()) {
                return out;
            }
        }
    }

    private static long displaySize(Activity activity, Uri uri, CancellationSignal signal) {
        try (Cursor cursor = activity.getContentResolver().query(uri,
                new String[] {OpenableColumns.SIZE}, null, null, null, signal)) {
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.SIZE);
                if (column >= 0 && !cursor.isNull(column)) return Math.max(0, cursor.getLong(column));
            }
        } catch (Exception error) {
            if (signal.isCanceled()) throw new android.os.OperationCanceledException();
        }
        return 0;
    }

    static final class CopyCancelled extends IOException {
        CopyCancelled() { super("selection was cancelled or replaced"); }
    }

    interface CopyControl {
        void update(long copiedBytes) throws IOException;
    }

    /** Actual stream-copy core, also exercised by the host bridge regression. */
    static void copyStream(InputStream input, File out, CopyControl control) throws IOException {
        boolean complete = false;
        try {
            try (FileOutputStream output = new FileOutputStream(out, false)) {
                byte[] buffer = new byte[1 << 16];
                long copied = 0;
                control.update(copied);
                for (;;) {
                    int read = input.read(buffer);
                    // Check again after a provider read: cancellation can close
                    // a blocked stream; returned bytes must not then be written.
                    control.update(copied);
                    if (read == -1) break;
                    if (read != 0) {
                        output.write(buffer, 0, read);
                        copied += read;
                        control.update(copied);
                    }
                }
                output.flush();
                output.getFD().sync();
                control.update(copied);
            }
            // A close failure also rejects the output, before it can be handed
            // to native code as a persistent, completely copied file.
            complete = true;
        } finally {
            if (!complete) out.delete();
        }
    }

    private void copy(Activity activity, Uri uri, File out, final int index, final int total,
            CancellationSignal signal) throws IOException {
        try {
            final long size = displaySize(activity, uri, signal);
            report(index, total, 0, size, true);
            try (AssetFileDescriptor descriptor = activity.getContentResolver().openAssetFileDescriptor(uri, "r", signal);
                 InputStream input = descriptor == null ? null : descriptor.createInputStream()) {
                if (input == null) throw new IOException("the provider returned no readable stream");
                activeInput = input;
                checkAllowed();
                report(index, total, 0, size, true);
                copyStream(input, out, new CopyControl() {
                    @Override public void update(long bytes) throws IOException {
                        report(index, total, bytes, size, false);
                    }
                });
                checkAllowed();
            }
        } catch (IOException | RuntimeException error) {
            out.delete();
            if (cancelled) throw new CopyCancelled();
            throw error;
        } finally {
            activeInput = null;
        }
    }
}
