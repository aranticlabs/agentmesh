from __future__ import annotations

from html.parser import HTMLParser
from pathlib import Path
import json
import re


ROOT = Path(__file__).resolve().parent.parent
WEBSITE = ROOT / "website"
HELP_SNAPSHOT = (
    ROOT
    / "crates"
    / "agentmesh-cli"
    / "src"
    / "snapshots"
    / "agentmesh__tests__command_help_surface.snap"
)


class Parser(HTMLParser):
    pass


class CommandParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.commands: dict[str, str] = {}
        self._current_summary: list[str] | None = None
        self._current_code: list[str] | None = None
        self._in_command = False
        self._in_summary = False
        self._in_code = False

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attributes = dict(attrs)
        classes = set((attributes.get("class") or "").split())
        if tag == "details" and "command" in classes:
            self._in_command = True
            self._current_summary = []
            self._current_code = []
        elif self._in_command and tag == "summary":
            self._in_summary = True
        elif self._in_command and tag == "code":
            self._in_code = True

    def handle_endtag(self, tag: str) -> None:
        if tag == "summary":
            self._in_summary = False
        elif tag == "code":
            self._in_code = False
        elif tag == "details" and self._in_command:
            summary = normalize_text("".join(self._current_summary or []))
            code = normalize_text("".join(self._current_code or []))
            if summary:
                self.commands[summary] = code
            self._in_command = False
            self._current_summary = None
            self._current_code = None

    def handle_data(self, data: str) -> None:
        if self._in_summary and self._current_summary is not None:
            self._current_summary.append(data)
        elif self._in_code and self._current_code is not None:
            self._current_code.append(data)


def normalize_text(value: str) -> str:
    lines = [line.rstrip() for line in value.strip().splitlines()]
    while lines and not lines[0]:
        lines.pop(0)
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def parse_help_snapshot() -> dict[str, str]:
    lines = HELP_SNAPSHOT.read_text().splitlines()
    fences = [index for index, line in enumerate(lines) if line == "---"]
    if len(fences) < 2:
        raise SystemExit("CLI help snapshot frontmatter is malformed")

    sections: dict[str, list[str]] = {}
    current: str | None = None
    for line in lines[fences[1] + 1 :]:
        if line.startswith("## "):
            current = line.removeprefix("## ").strip()
            sections[current] = []
        elif current is not None:
            sections[current].append(line)

    return {command: normalize_text("\n".join(body)) for command, body in sections.items()}


def parse_command_page() -> dict[str, str]:
    parser = CommandParser()
    parser.feed((WEBSITE / "commands.html").read_text())
    return parser.commands


def check_html_parse() -> None:
    for path in sorted(WEBSITE.glob("*.html")):
        Parser().feed(path.read_text())


def check_ascii() -> None:
    for path in sorted(WEBSITE.glob("*")):
        if path.is_file():
            path.read_text(encoding="ascii")


def check_links() -> None:
    missing = []
    for path in WEBSITE.glob("*.html"):
        for href in re.findall(r'href="([^"]+)"', path.read_text()):
            if href.startswith(("http://", "https://", "#", "mailto:")):
                continue
            target = href.split("#", 1)[0]
            if target and not (path.parent / target).exists():
                missing.append((str(path.relative_to(ROOT)), href))
    if missing:
        raise SystemExit(f"missing website links: {missing}")


def check_commands() -> None:
    expected = parse_help_snapshot()
    actual = parse_command_page()

    if expected.keys() != actual.keys():
        missing = sorted(expected.keys() - actual.keys())
        extra = sorted(actual.keys() - expected.keys())
        raise SystemExit(f"website command sections differ from help snapshot; missing={missing} extra={extra}")

    mismatched = [
        command
        for command, expected_help in expected.items()
        if actual[command] != expected_help
    ]
    if mismatched:
        raise SystemExit(f"website command help content is stale: {mismatched}")


def check_install_commands() -> None:
    npm_package = json.loads((ROOT / "installers" / "npm" / "package.json").read_text())["name"]
    index = (WEBSITE / "index.html").read_text()
    if f"npm install -g {npm_package}" not in index:
        raise SystemExit("website npm install command does not match package metadata")
    if "curl -fsSL https://agentmesh.sh/install.sh | sh" not in index:
        raise SystemExit("website macOS/Linux install command is missing")
    if "irm https://agentmesh.sh/install.ps1 | iex" not in index:
        raise SystemExit("website Windows install command is missing")


def main() -> None:
    check_html_parse()
    check_ascii()
    check_links()
    check_commands()
    check_install_commands()


if __name__ == "__main__":
    main()
