"""AppImage only: undo the GUI launcher's environment for child processes.

AppRun points LD_LIBRARY_PATH / PYTHONHOME / PYTHONPATH at the bundled runtime for this process;
the loader and the interpreter have already consumed them, so restore the caller's values here and
the GUI's children (ldm, ldmd -> ffmpeg / yt-dlp, sound players) run with the host environment."""

import os

if os.environ.pop("LDM_AI_RESTORE", None):
    for _v in ("LD_LIBRARY_PATH", "PYTHONHOME", "PYTHONPATH", "PYTHONNOUSERSITE"):
        _saved = os.environ.pop("LDM_AI_SAVED_" + _v, None)
        if _saved is None:
            os.environ.pop(_v, None)
        else:
            os.environ[_v] = _saved
