package io.github.ydog12138.liteavd.audiofixture;

import android.app.Activity;
import android.graphics.Color;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioTrack;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.Gravity;
import android.view.WindowManager;
import android.widget.TextView;
import java.io.File;
import java.io.FileOutputStream;
import java.io.PrintWriter;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public final class MainActivity extends Activity {
    private static final int SAMPLE_RATE = 48000;
    private static final int CALLBACK_FRAMES = 960;
    private volatile boolean running;
    private Thread worker;
    private TextView label;

    @Override
    public void onCreate(Bundle state) {
        super.onCreate(state);
        int frequency = Math.max(100, Math.min(2000, getIntent().getIntExtra("frequency", 440)));
        int periodMs = Math.max(0, getIntent().getIntExtra("period_ms", 0));
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        label = new TextView(this);
        label.setTextSize(28);
        label.setTextColor(Color.WHITE);
        label.setGravity(Gravity.CENTER);
        showState(frequency);
        setContentView(label);
        running = true;
        worker = new Thread(() -> play(frequency, periodMs), "liteavd-audio-fixture");
        worker.start();
    }

    @Override
    protected void onDestroy() {
        running = false;
        super.onDestroy();
    }

    private void play(int initialFrequency, int periodMs) {
        File ready = new File(getFilesDir(), "ready");
        File failure = new File(getFilesDir(), "failure.txt");
        ready.delete();
        failure.delete();
        AudioTrack track = null;
        try {
            int minimum = AudioTrack.getMinBufferSize(
                    SAMPLE_RATE,
                    AudioFormat.CHANNEL_OUT_STEREO,
                    AudioFormat.ENCODING_PCM_16BIT);
            if (minimum <= 0) {
                throw new IllegalStateException("invalid AudioTrack minimum buffer " + minimum);
            }
            track = new AudioTrack.Builder()
                    .setAudioAttributes(new AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_MEDIA)
                            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                            .build())
                    .setAudioFormat(new AudioFormat.Builder()
                            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                            .setSampleRate(SAMPLE_RATE)
                            .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                            .build())
                    .setBufferSizeInBytes(Math.max(minimum, CALLBACK_FRAMES * 4 * 2))
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .build();
            if (track.getState() != AudioTrack.STATE_INITIALIZED) {
                throw new IllegalStateException("AudioTrack did not initialize");
            }
            short[] pcm = new short[CALLBACK_FRAMES * 2];
            int frequency = initialFrequency;
            double phase = 0.0;
            track.play();
            boolean markedReady = false;
            long framesWritten = 0;
            long nextSwitchMs = SystemClock.elapsedRealtime() + periodMs;
            while (running) {
                long transitionFrame = -1;
                if (periodMs > 0 && SystemClock.elapsedRealtime() >= nextSwitchMs) {
                    frequency = frequency < 660 ? 880 : 440;
                    transitionFrame = framesWritten;
                    nextSwitchMs = SystemClock.elapsedRealtime() + periodMs;
                }
                double increment = Math.PI * 2.0 * frequency / SAMPLE_RATE;
                for (int frame = 0; frame < CALLBACK_FRAMES; frame++) {
                    short sample = (short) Math.round(Math.sin(phase) * 10000.0);
                    pcm[frame * 2] = sample;
                    pcm[frame * 2 + 1] = sample;
                    phase += increment;
                    if (phase >= Math.PI * 2.0) {
                        phase -= Math.PI * 2.0;
                    }
                }
                int offset = 0;
                while (running && offset < pcm.length) {
                    int count = track.write(pcm, offset, pcm.length - offset, AudioTrack.WRITE_BLOCKING);
                    if (count <= 0) {
                        throw new IllegalStateException("AudioTrack.write failed: " + count);
                    }
                    if ((count & 1) != 0) {
                        throw new IllegalStateException("AudioTrack.write returned partial stereo frame");
                    }
                    offset += count;
                    framesWritten += count / 2;
                }
                if (transitionFrame >= 0) {
                    while (running
                            && Integer.toUnsignedLong(track.getPlaybackHeadPosition())
                                    < transitionFrame) {
                        SystemClock.sleep(1);
                    }
                    showStateAndWait(frequency);
                }
                if (!markedReady) {
                    touch(ready);
                    markedReady = true;
                }
            }
        } catch (Throwable error) {
            try (PrintWriter writer = new PrintWriter(failure)) {
                error.printStackTrace(writer);
            } catch (Throwable ignored) {
            }
        } finally {
            if (track != null) {
                try {
                    track.pause();
                    track.flush();
                    track.stop();
                } catch (Throwable ignored) {
                }
                track.release();
            }
        }
    }

    private static int colorFor(int frequency) {
        if (frequency < 550) {
            return Color.rgb(190, 35, 45);
        }
        if (frequency < 770) {
            return Color.rgb(30, 145, 70);
        }
        return Color.rgb(35, 90, 190);
    }

    private void showState(int frequency) {
        label.setText("liteavd audio fixture\n" + frequency + " Hz");
        label.setBackgroundColor(colorFor(frequency));
    }

    private void showStateAndWait(int frequency) throws Exception {
        CountDownLatch displayed = new CountDownLatch(1);
        runOnUiThread(() -> {
            showState(frequency);
            displayed.countDown();
        });
        if (!displayed.await(1, TimeUnit.SECONDS)) {
            throw new IllegalStateException("UI state transition timed out");
        }
    }

    private static void touch(File file) throws Exception {
        try (FileOutputStream output = new FileOutputStream(file)) {
            output.getFD().sync();
        }
    }
}
