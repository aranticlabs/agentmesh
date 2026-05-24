from __future__ import annotations

import hashlib
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

VERSION = os.environ.get("AGENTMESH_VERSION", "0.1.0")
BASE_URL = os.environ.get(
    "AGENTMESH_BASE_URL",
    "https://github.com/aranticlabs/agentmesh/releases/download",
)
CHANNEL = os.environ.get("AGENTMESH_CHANNEL", "stable")


def detect_platform() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()
    if system == "darwin":
        os_name = "apple-darwin"
    elif system == "linux":
        os_name = "unknown-linux-gnu"
    elif system.startswith(("msys", "mingw", "cygwin")) or system == "windows":
        os_name = "pc-windows-msvc"
    else:
        raise SystemExit(f"unsupported operating system: {system}")

    if machine in {"x86_64", "amd64"}:
        arch = "x86_64"
    elif machine in {"arm64", "aarch64"}:
        arch = "aarch64"
    else:
        raise SystemExit(f"unsupported architecture: {machine}")

    return f"{arch}-{os_name}"


def release_tag(channel: str = CHANNEL) -> str:
    if channel == "stable":
        return f"agentmesh-cli-v{VERSION}"
    if channel == "nightly":
        return "nightly"
    raise SystemExit(f"unsupported channel: {channel}")


def artifact_name(target: str | None = None, channel: str = CHANNEL) -> str:
    target = target or detect_platform()
    if channel == "stable":
        return f"agentmesh-v{VERSION}-{target}.tar.gz"
    if channel == "nightly":
        return f"agentmesh-nightly-{target}.tar.gz"
    raise SystemExit(f"unsupported channel: {channel}")


def artifact_url(channel: str = CHANNEL) -> str:
    tag = release_tag(channel)
    artifact = artifact_name(channel=channel)
    return f"{BASE_URL}/{tag}/{artifact}"


def manifest_url(channel: str = CHANNEL) -> str:
    return f"{BASE_URL}/{release_tag(channel)}/SHA256SUMS"


def signature_url(channel: str = CHANNEL) -> str:
    return f"{BASE_URL}/{release_tag(channel)}/SHA256SUMS.sig"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def manifest_hash_for(manifest: Path, artifact: str) -> str | None:
    basename = Path(artifact).name
    for line in manifest.read_text(encoding="utf-8").splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        candidate = parts[1].removeprefix("*").removeprefix("./")
        if parts[1] == artifact or candidate == basename:
            return parts[0]
    return None


def verify_sha256(path: Path, expected: str) -> None:
    actual = sha256_file(path)
    if actual != expected:
        raise SystemExit(
            f"sha256 mismatch for {path}\n  expected: {expected}\n  actual:   {actual}"
        )
    print(f"sha256 verified: {path}")


def verify_sha256sums(path: Path, manifest: Path, artifact: str) -> None:
    expected = manifest_hash_for(manifest, artifact)
    if expected is None:
        raise SystemExit(f"artifact {artifact} not found in {manifest}")
    verify_sha256(path, expected)


def verify_manifest_signature(manifest: Path, signature: Path) -> None:
    cosign = shutil.which("cosign")
    if cosign is None:
        raise SystemExit("cosign is required to verify SHA256SUMS signatures")
    subprocess.run(
        [cosign, "verify-blob", "--signature", str(signature), str(manifest)],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    print(f"signature verified: {manifest}")


def binary_dir() -> Path:
    override = os.environ.get("AGENTMESH_PIP_BINARY_DIR")
    if override:
        return Path(override)
    return Path(__file__).resolve().parent.parent / "agentmesh_bin"


def binary_path() -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    return binary_dir() / f"agentmesh{suffix}"


def download(url: str, output: Path) -> None:
    with urllib.request.urlopen(url) as response:
        output.write_bytes(response.read())


def extract_tar_safely(archive: Path, destination: Path) -> None:
    destination = destination.resolve()
    with tarfile.open(archive, "r:gz") as archive_file:
        for member in archive_file.getmembers():
            target = (destination / member.name).resolve()
            if not target.is_relative_to(destination):
                raise SystemExit(f"archive member escapes extraction directory: {member.name}")
            if member.issym() or member.islnk() or member.isdev():
                raise SystemExit(f"unsupported archive member type: {member.name}")
        archive_file.extractall(destination)


def install_binary() -> None:
    target = detect_platform()
    artifact = artifact_name(target)
    tag = release_tag()
    with tempfile.TemporaryDirectory(prefix="agentmesh-pip-install-") as temp:
        temp_path = Path(temp)
        archive = temp_path / artifact
        manifest = temp_path / "SHA256SUMS"
        signature = temp_path / "SHA256SUMS.sig"
        download(f"{BASE_URL}/{tag}/SHA256SUMS", manifest)
        download(f"{BASE_URL}/{tag}/SHA256SUMS.sig", signature)
        download(f"{BASE_URL}/{tag}/{artifact}", archive)
        verify_manifest_signature(manifest, signature)
        verify_sha256sums(archive, manifest, artifact)
        extract_dir = temp_path / "extract"
        extract_dir.mkdir()
        extract_tar_safely(archive, extract_dir)
        candidates = [
            path
            for path in extract_dir.rglob("agentmesh.exe" if os.name == "nt" else "agentmesh")
            if path.is_file() and not path.is_symlink()
        ]
        if not candidates:
            raise SystemExit(f"agentmesh binary not found in {artifact}")
        destination = binary_path()
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(candidates[0], destination)
        destination.chmod(destination.stat().st_mode | 0o755)
        print(f"Installed agentmesh binary to {destination}")


def print_help() -> None:
    print(
        "AgentMesh pipx wrapper\n\n"
        "Usage:\n"
        "  agentmesh --smoke\n"
        "  agentmesh --print-platform\n"
        "  agentmesh --print-url\n"
        "  agentmesh --verify-sha256 <file> <expected-sha256>\n"
        "  agentmesh --verify-sha256sums <file> <SHA256SUMS> <artifact-name>\n"
        "  agentmesh --verify-sha256sums-signature <SHA256SUMS> <SHA256SUMS.sig>\n"
        "  agentmesh --install\n"
        "  agentmesh --upgrade-help\n\n"
        "Without a wrapper flag, this command installs the signed, verified binary if needed and delegates to it."
    )


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] in {"-h", "--help"}:
        print_help()
        return 0
    if args == ["--smoke"]:
        target = detect_platform()
        print(f"agentmesh pipx wrapper smoke ok (platform={target} artifact={artifact_name(target)})")
        return 0
    if args == ["--print-platform"]:
        print(detect_platform())
        return 0
    if args == ["--print-url"]:
        print(f"artifact_url={artifact_url()}")
        print(f"sha256sums_url={manifest_url()}")
        print(f"signature_url={signature_url()}")
        return 0
    if args and args[0] == "--verify-sha256":
        if len(args) != 3:
            print("usage: agentmesh --verify-sha256 <file> <expected-sha256>", file=sys.stderr)
            return 64
        verify_sha256(Path(args[1]), args[2])
        return 0
    if args and args[0] == "--verify-sha256sums":
        if len(args) != 4:
            print(
                "usage: agentmesh --verify-sha256sums <file> <SHA256SUMS> <artifact-name>",
                file=sys.stderr,
            )
            return 64
        verify_sha256sums(Path(args[1]), Path(args[2]), args[3])
        return 0
    if args and args[0] == "--verify-sha256sums-signature":
        if len(args) != 3:
            print(
                "usage: agentmesh --verify-sha256sums-signature <SHA256SUMS> <SHA256SUMS.sig>",
                file=sys.stderr,
            )
            return 64
        verify_manifest_signature(Path(args[1]), Path(args[2]))
        return 0
    if args == ["--install"]:
        install_binary()
        return 0
    if args == ["--upgrade-help"]:
        print(
            "AgentMesh upgrade flow:\n"
            "  1. Update agentmesh with pipx.\n"
            "  2. Run `agentmesh upgrade` in each repository that has hooks installed.\n"
            "  3. Review the printed path and hash summary before continuing hook-driven sync."
        )
        return 0
    if args == ["--version"] and not binary_path().exists():
        print("agentmesh pipx wrapper 0.1.0 (binary not installed)")
        return 0

    if not binary_path().exists():
        install_binary()

    completed = subprocess.run([str(binary_path()), *args], check=False)
    return completed.returncode
