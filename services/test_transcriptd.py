#!/usr/bin/env python3
"""Tests for transcriptd.fetch against a fake yt-dlp — no network, stdlib only.

    python3 services/test_transcriptd.py

The fake writes the caption files and stderr a real yt-dlp run produced
for each case, so what is pinned here is the service's reading of them:
which file wins, and which `kind` a failure becomes.
"""
import json, os, stat, sys, tempfile, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcriptd  # noqa: E402

VID = "abcDEF12345"
URL = f"https://www.youtube.com/watch?v={VID}"
VTT = "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\n{}\n"

# Prints the --print-json line, writes <template>.<lang>.vtt for each
# FAKE_FILES entry ("lang=text"), echoes FAKE_STDERR, exits FAKE_RC.
FAKE = r'''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
out = args[args.index("-o") + 1].replace(".%(ext)s", "")
manual = [l for l in os.environ.get("FAKE_MANUAL", "").split(",") if l]
print(json.dumps({"title": "Fake", "duration": 60,
                  "subtitles": {l: [] for l in manual}}))
for spec in [s for s in os.environ.get("FAKE_FILES", "").split(";") if s]:
    lang, text = spec.split("=", 1)
    with open(f"{out}.{lang}.vtt", "w") as fh:
        fh.write("WEBVTT\n\n00:00:00.000 --> 00:00:02.000\n" + text + "\n")
sys.stderr.write(os.environ.get("FAKE_STDERR", ""))
sys.exit(int(os.environ.get("FAKE_RC", "0")))
'''

# What yt-dlp 2026.08.19 printed with --ignore-errors when YouTube refused
# the translated en track (reproduced 2026-10-07).
WARN_429 = "WARNING: Unable to download video subtitles for 'en': HTTP Error 429: Too Many Requests\n"


class FetchTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        fake = os.path.join(self.dir, "yt-dlp")
        with open(fake, "w") as fh:
            fh.write(FAKE)
        os.chmod(fake, os.stat(fake).st_mode | stat.S_IEXEC)
        self.cache = os.path.join(self.dir, "cache")
        os.makedirs(self.cache)
        self._saved = (transcriptd.YTDLP, transcriptd.CACHE)
        transcriptd.YTDLP, transcriptd.CACHE = fake, self.cache
        for k in ("FAKE_FILES", "FAKE_MANUAL", "FAKE_STDERR", "FAKE_RC"):
            os.environ.pop(k, None)

    def tearDown(self):
        transcriptd.YTDLP, transcriptd.CACHE = self._saved
        # Every path, success or failure, must leave no .tmp-* behind.
        self.assertEqual(os.listdir(self.cache), [])

    def run_fetch(self, files="", manual="", stderr="", rc=0):
        os.environ.update(FAKE_FILES=files, FAKE_MANUAL=manual,
                          FAKE_STDERR=stderr, FAKE_RC=str(rc))
        return transcriptd.fetch(URL, VID)

    def kind_of(self, **kw):
        with self.assertRaises(RuntimeError) as cm:
            self.run_fetch(**kw)
        return cm.exception.args[0]

    def test_429_on_translated_track_does_not_hide_en_orig(self):
        # The 2026-10-07 failure: en-orig arrived, en was refused.
        got = self.run_fetch(files="en-orig=what was said", stderr=WARN_429)
        self.assertEqual(got["text"], "what was said")
        self.assertEqual(got["title"], "Fake")

    def test_uploader_captions_beat_en_orig(self):
        got = self.run_fetch(files="en-orig=recognised;en=written", manual="en")
        self.assertEqual(got["text"], "written")

    def test_en_orig_beats_other_auto_tracks(self):
        got = self.run_fetch(files="en-GB=other;en-orig=recognised")
        self.assertEqual(got["text"], "recognised")

    def test_429_with_no_file_is_rate_limited(self):
        # With --ignore-errors the refusal is a WARNING and the exit is 0.
        self.assertEqual(self.kind_of(stderr=WARN_429), "rate_limited")

    def test_hard_error_reports_the_error_line_not_a_warning(self):
        with self.assertRaises(RuntimeError) as cm:
            self.run_fetch(stderr="ERROR: [youtube] abcDEF12345: This video is unavailable\n"
                                  "WARNING: something after it\n", rc=1)
        kind, msg = cm.exception.args
        self.assertEqual(kind, "failed")
        self.assertIn("This video is unavailable", msg)

    def test_nothing_written_is_no_captions(self):
        self.assertEqual(self.kind_of(), "no_captions")

    def test_command_asks_for_en_orig_first_and_keeps_warnings(self):
        seen = {}
        real_run = transcriptd.subprocess.run

        def spy(cmd, **kw):
            seen["cmd"] = cmd
            return real_run(cmd, **kw)

        transcriptd.subprocess.run = spy
        try:
            self.run_fetch(files="en-orig=x")
        finally:
            transcriptd.subprocess.run = real_run
        cmd = seen["cmd"]
        self.assertTrue(cmd[cmd.index("--sub-langs") + 1].startswith("en-orig,"))
        self.assertIn("--ignore-errors", cmd)
        self.assertNotIn("--no-warnings", cmd)   # it would hide the 429


if __name__ == "__main__":
    unittest.main(verbosity=2)
