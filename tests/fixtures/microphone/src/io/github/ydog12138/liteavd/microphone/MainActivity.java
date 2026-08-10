package io.github.ydog12138.liteavd.microphone;

import android.Manifest;
import android.app.Activity;
import android.content.pm.PackageManager;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.media.MediaRecorder;
import android.os.Bundle;
import android.os.SystemClock;
import android.widget.TextView;
import java.io.File;
import java.io.FileOutputStream;
import java.io.PrintWriter;

public final class MainActivity extends Activity {
    private static final int SAMPLE_RATE = 48000;
    private static final long RECORDING_MS = 8000;

    @Override
    public void onCreate(Bundle state) {
        super.onCreate(state);
        TextView message = new TextView(this);
        message.setText("liteavd microphone fixture is recording");
        setContentView(message);
        new Thread(this::record, "liteavd-audio-record").start();
    }

    private void record() {
        File directory = getFilesDir();
        File capture = new File(directory, "capture.pcm");
        File ready = new File(directory, "ready");
        File done = new File(directory, "done");
        File failure = new File(directory, "failure.txt");
        capture.delete();
        ready.delete();
        done.delete();
        failure.delete();

        AudioRecord recorder = null;
        try {
            if (checkSelfPermission(Manifest.permission.RECORD_AUDIO)
                    != PackageManager.PERMISSION_GRANTED) {
                throw new IllegalStateException("RECORD_AUDIO was not granted");
            }
            int minimum = AudioRecord.getMinBufferSize(
                    SAMPLE_RATE,
                    AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT);
            if (minimum <= 0) {
                throw new IllegalStateException("invalid AudioRecord minimum buffer " + minimum);
            }
            recorder = new AudioRecord(
                    MediaRecorder.AudioSource.MIC,
                    SAMPLE_RATE,
                    AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT,
                    Math.max(minimum * 2, 8192));
            if (recorder.getState() != AudioRecord.STATE_INITIALIZED) {
                throw new IllegalStateException("AudioRecord did not initialize");
            }
            recorder.startRecording();
            if (recorder.getRecordingState() != AudioRecord.RECORDSTATE_RECORDING) {
                throw new IllegalStateException("AudioRecord did not start");
            }
            touch(ready);

            byte[] buffer = new byte[Math.max(minimum, 4096)];
            long deadline = SystemClock.elapsedRealtime() + RECORDING_MS;
            try (FileOutputStream output = new FileOutputStream(capture)) {
                while (SystemClock.elapsedRealtime() < deadline) {
                    int count = recorder.read(buffer, 0, buffer.length, AudioRecord.READ_BLOCKING);
                    if (count < 0) {
                        throw new IllegalStateException("AudioRecord.read failed: " + count);
                    }
                    if (count > 0) {
                        output.write(buffer, 0, count);
                    }
                }
                output.getFD().sync();
            }
            touch(done);
        } catch (Throwable error) {
            try (PrintWriter writer = new PrintWriter(failure)) {
                error.printStackTrace(writer);
            } catch (Throwable ignored) {
                // The host test will report a missing marker if even failure output is impossible.
            }
        } finally {
            if (recorder != null) {
                try {
                    recorder.stop();
                } catch (Throwable ignored) {
                }
                recorder.release();
            }
        }
    }

    private static void touch(File file) throws Exception {
        try (FileOutputStream output = new FileOutputStream(file)) {
            output.getFD().sync();
        }
    }
}
