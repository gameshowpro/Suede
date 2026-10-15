"""Copy locally hosted documentation fonts into MkDocs' generated assets."""

from pathlib import Path
import shutil


def on_pre_build(config):
    """Copy the canonical font package into the docs tree before MkDocs reads it."""
    project_root = Path(config["config_file_path"]).resolve().parent
    source = project_root / "fonts"
    destination = project_root / "docs" / "generated" / "fonts"

    if not source.is_dir():
        raise FileNotFoundError(f"Font assets directory not found: {source}")

    destination.mkdir(parents=True, exist_ok=True)
    for asset in source.iterdir():
        if asset.is_file() and asset.suffix in {".woff2", ".css", ".txt"}:
            shutil.copy2(asset, destination / asset.name)
