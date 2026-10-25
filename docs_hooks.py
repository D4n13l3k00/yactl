"""Publish installation examples without maintaining copies in docs/."""
from pathlib import Path
import posixpath
from mkdocs.structure.files import File

ROOT = Path(__file__).resolve().parent
DOWNLOADS = {
    "deploy/S99yactl": "downloads/S99yactl",
    "deploy/settings-midi.example.json": "downloads/settings-midi.example.json",
    "deploy/yactl.fish": "downloads/yactl.fish",
    "LICENSE": "downloads/LICENSE",
    ".github/workflows/checks.yml": "downloads/checks.yml",
    ".github/workflows/docs.yml": "downloads/docs.yml",
}


def on_files(files, config):
    for source, destination in DOWNLOADS.items():
        files.append(File.generated(config, destination, abs_src_path=str(ROOT / source)))
    return files


def on_page_markdown(markdown, page, **kwargs):
    parent = posixpath.dirname(page.file.src_uri)
    for source, destination in DOWNLOADS.items():
        original = posixpath.relpath("../" + source, parent or ".")
        replacement = posixpath.relpath(destination, parent or ".")
        markdown = markdown.replace(f"]({original})", f"]({replacement})")
    return markdown


def on_config(config):
    if not config.repo_url:
        config.edit_uri = ""
    return config
