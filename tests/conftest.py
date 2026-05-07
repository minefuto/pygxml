import sys
from pathlib import Path

# Allow running pytest without installing the package in editable mode
ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "python"))
