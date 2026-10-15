"""Generate the canonical artwork assets before MkDocs builds the site."""

from pathlib import Path
import hashlib
import json
import subprocess

from mkdocs.exceptions import PluginError


def file_hash(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def on_pre_build(config):
    """Run the Rust artwork generator from the repository root."""
    repo_root = Path(__file__).resolve().parent.parent
    output = repo_root / "docs/generated/artwork"
    cache = repo_root / "docs/generated/.artwork-cache.json"
    sources = [repo_root / "Cargo.toml", repo_root / "Cargo.lock", Path(__file__)]
    sources += sorted((repo_root / "artwork").glob("*"))
    sources += sorted((repo_root / "scripts/artwork").rglob("*.rs"))
    sources.append(repo_root / "scripts/artwork/Cargo.toml")
    signature = {str(path.relative_to(repo_root)): file_hash(path) for path in sources if path.is_file()}
    try:
        previous = json.loads(cache.read_text())
        if previous["sources"] == signature and previous["outputs"] and all(
            (output / name).is_file() and file_hash(output / name) == digest
            for name, digest in previous["outputs"].items()
        ):
            return
    except (OSError, ValueError, KeyError, TypeError):
        pass

    command = ["cargo", "run", "--quiet", "--locked", "-p", "suede-artwork", "--", "docs"]
    try:
        result = subprocess.run(command, cwd=repo_root, text=True, capture_output=True)
    except OSError as error:
        raise PluginError(f"Could not launch the artwork generator: {error}") from error
    if result.returncode:
        details = (result.stderr or result.stdout).strip()
        raise PluginError(
            "Could not generate documentation artwork with "
            f"`{' '.join(command)}` (exit {result.returncode}).\n{details}"
        )
    if result.stdout.strip():
        print(result.stdout, end="")
    # Text-only documentation edits should not repeatedly resize the artwork.
    # Hash outputs too, so deleting or changing a derivative forces regeneration.
    outputs = {path.name: file_hash(path) for path in sorted(output.iterdir()) if path.is_file()}
    cache.write_text(json.dumps({"sources": signature, "outputs": outputs}, indent=2) + "\n")
