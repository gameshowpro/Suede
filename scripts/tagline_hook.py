"""Derive the site description from the tagline in Cargo.toml.

The tagline is written once, as `description` in Cargo.toml. This exposes it to
templates as `config.extra.tagline` and builds `site_description` from it and
`extra.description_detail`.
"""

from pathlib import Path
import tomllib


def on_config(config):
    """Read the tagline and fill in the settings that derive from it."""
    project_root = Path(config["config_file_path"]).resolve().parent
    with open(project_root / "Cargo.toml", "rb") as manifest:
        tagline = tomllib.load(manifest)["package"]["description"]

    config["extra"]["tagline"] = tagline
    detail = config["extra"].get("description_detail", "")
    config["site_description"] = f"{tagline} {detail}".strip()
    return config
