#!/usr/bin/env python3
"""Prepare entry for axis D: install the faults named in CAPABILITY_FAULTS."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from lib.faults import main  # noqa: E402

raise SystemExit(main())
