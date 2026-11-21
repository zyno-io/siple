#!/usr/bin/env python3
"""Strict cross-check of the generated Flatpak cargo sources against Cargo.lock.

    flatpak-cargo-check.py LOCK SOURCES.json    exit 1 with a reason when anything is off
    flatpak-cargo-check.py --self-test          negative cases (must all fail) and a positive one

The generated file is executed by the build (archives are unpacked into the vendor directory and the
cargo config decides where cargo reads crates from), so it must hold exactly this, nothing else:
  - per registry package of the lock: one `archive` (the lock's crates.io URL and sha256, dest
    cargo/vendor/<name>-<version>) and one `inline` `.cargo-checksum.json` at the same dest whose
    contents are exactly {"package": <lock checksum>, "files": {}};
  - one `inline` `config` in `cargo` with the vendoring config below, byte for byte.
Needs Python >= 3.11 (tomllib). Run with -I.
"""
import json
import re
import sys
import tomllib

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
URL_RX = re.compile(r"^https://static\.crates\.io/crates/([^/]+)/\1-(.+)\.crate$")
CONFIG_CONTENTS = (
    '[source.vendored-sources]\ndirectory = "cargo/vendor"\n\n'
    "[source.crates-io]\nreplace-with = \"vendored-sources\"\n"
)
ARCHIVE_KEYS = {"type", "archive-type", "url", "sha256", "dest"}
INLINE_KEYS = {"type", "contents", "dest", "dest-filename"}


class Bad(Exception):
    pass


def lock_packages(lock):
    want = {}
    for p in lock["package"]:
        src = p.get("source")
        if src is None:
            continue  # workspace or path crate
        label = f"{p['name']} {p['version']}"
        if src.startswith("git+"):
            raise Bad(f"{label} is a git dependency: the offline sources do not cover git")
        if src != REGISTRY:
            raise Bad(f"{label} comes from an unknown registry: {src}")
        chk = p.get("checksum")
        if not isinstance(chk, str) or not re.fullmatch(r"[0-9a-f]{64}", chk):
            raise Bad(f"{label} has no valid checksum in the lock file")
        want[(p["name"], p["version"])] = chk
    return want


def check(lock, sources):
    """Raises Bad unless `sources` is exactly what the lock file calls for."""
    want = lock_packages(lock)
    if not isinstance(sources, list):
        raise Bad("the sources are not a JSON list")
    by_dest = {f"cargo/vendor/{n}-{v}": (n, v) for n, v in want}
    archives, sums, configs = {}, {}, []
    for s in sources:
        if not isinstance(s, dict):
            raise Bad(f"entry is not an object: {s!r}")
        t = s.get("type")
        if t == "archive":
            if set(s) != ARCHIVE_KEYS:
                raise Bad(f"archive with unexpected keys {sorted(s)}: {s.get('url')}")
            m = URL_RX.fullmatch(s["url"]) if isinstance(s["url"], str) else None
            if not m:
                raise Bad(f"unexpected archive url {s['url']!r}")
            key = (m.group(1), m.group(2))
            if key not in want:
                raise Bad(f"archive for {key} which is not in the lock file")
            if key in archives:
                raise Bad(f"duplicate archive for {key}")
            if s["archive-type"] != "tar-gzip":
                raise Bad(f"archive {key}: archive-type {s['archive-type']!r}")
            if s["sha256"] != want[key]:
                raise Bad(f"archive {key}: sha256 differs from the lock file")
            if s["dest"] != f"cargo/vendor/{key[0]}-{key[1]}":
                raise Bad(f"archive {key}: dest {s['dest']!r}")
            archives[key] = True
        elif t == "inline":
            if set(s) != INLINE_KEYS:
                raise Bad(f"inline with unexpected keys {sorted(s)}")
            name, dest, body = s["dest-filename"], s["dest"], s["contents"]
            if name == "config":
                if dest != "cargo":
                    raise Bad(f"cargo config at {dest!r}")
                if body != CONFIG_CONTENTS:
                    raise Bad("the cargo config differs from the expected text")
                configs.append(s)
            elif name == ".cargo-checksum.json":
                key = by_dest.get(dest) if isinstance(dest, str) else None
                if key is None:
                    raise Bad(f"checksum file at {dest!r} which is no crate of the lock file")
                if key in sums:
                    raise Bad(f"duplicate checksum file for {key}")
                try:
                    parsed = json.loads(body)
                except (TypeError, ValueError):
                    raise Bad(f"checksum file of {key} is not JSON")
                if parsed != {"package": want[key], "files": {}}:
                    raise Bad(f"checksum file of {key} does not hold exactly the lock's checksum and no files")
                sums[key] = True
            else:
                raise Bad(f"unexpected inline file {name!r} in {dest!r}")
        else:
            raise Bad(f"unknown source type {t!r} in the generated sources")
    if len(configs) != 1:
        raise Bad(f"expected exactly one cargo config, found {len(configs)}")
    for what, got in (("archive", archives), ("checksum file", sums)):
        if set(got) != set(want):
            missing = sorted(set(want) - set(got))[:5]
            raise Bad(f"missing {what}s: {missing}")
    return len(want)


def self_test():
    import copy

    sha = lambda c: c * 64
    lock = {
        "package": [
            {"name": "siple", "version": "0.1.0"},
            {"name": "a", "version": "1.0.0", "source": REGISTRY, "checksum": sha("a")},
            {"name": "b-c", "version": "2.1.0-rc.1", "source": REGISTRY, "checksum": sha("b")},
        ]
    }

    def good():
        out = []
        for n, v, c in (("a", "1.0.0", "a"), ("b-c", "2.1.0-rc.1", "b")):
            dest = f"cargo/vendor/{n}-{v}"
            out.append({"type": "archive", "archive-type": "tar-gzip",
                        "url": f"https://static.crates.io/crates/{n}/{n}-{v}.crate",
                        "sha256": sha(c), "dest": dest})
            out.append({"type": "inline", "contents": json.dumps({"package": sha(c), "files": {}}),
                        "dest": dest, "dest-filename": ".cargo-checksum.json"})
        out.append({"type": "inline", "contents": CONFIG_CONTENTS, "dest": "cargo", "dest-filename": "config"})
        return out

    check(lock, good())  # the positive case

    def mutate(f):
        s = good()
        f(s)
        return s

    cfg = lambda s: s[-1]
    cases = {
        "extra inline file in a crate dir": lambda s: s.append(
            {"type": "inline", "contents": "x", "dest": "cargo/vendor/a-1.0.0", "dest-filename": "build.rs"}),
        "swapped archive dests": lambda s: (
            s[0].__setitem__("dest", "cargo/vendor/b-c-2.1.0-rc.1"),
            s[2].__setitem__("dest", "cargo/vendor/a-1.0.0")),
        "extra [build] rustflags in the cargo config": lambda s: cfg(s).__setitem__(
            "contents", CONFIG_CONTENTS + '\n[build]\nrustflags = ["-C", "link-arg=-evil"]\n'),
        "cargo config in the wrong dest": lambda s: cfg(s).__setitem__("dest", "cargo/vendor"),
        "second cargo config": lambda s: s.append(copy.deepcopy(cfg(s))),
        "no cargo config": lambda s: s.pop(),
        "wrong sha256": lambda s: s[0].__setitem__("sha256", sha("f")),
        "wrong archive url host": lambda s: s[0].__setitem__("url", "https://evil.example/crates/a/a-1.0.0.crate"),
        "duplicate archive": lambda s: s.append(copy.deepcopy(s[0])),
        "unknown crate archive": lambda s: s.append(
            {**copy.deepcopy(s[0]), "url": "https://static.crates.io/crates/z/z-9.9.9.crate",
             "dest": "cargo/vendor/z-9.9.9"}),
        "missing archive": lambda s: s.pop(2),
        "missing checksum file": lambda s: s.pop(1),
        "duplicate checksum file": lambda s: s.append(copy.deepcopy(s[1])),
        "checksum lists files": lambda s: s[1].__setitem__(
            "contents", json.dumps({"package": sha("a"), "files": {"x": sha("0")}})),
        "checksum of another package": lambda s: s[1].__setitem__(
            "contents", json.dumps({"package": sha("b"), "files": {}})),
        "checksum with an extra key": lambda s: s[1].__setitem__(
            "contents", json.dumps({"package": sha("a"), "files": {}, "x": 1})),
        "checksum contents not JSON": lambda s: s[1].__setitem__("contents", "nope"),
        "checksum file in the wrong dest": lambda s: s[1].__setitem__("dest", "cargo/vendor/b-c-2.1.0-rc.1"),
        "unknown key on an archive": lambda s: s[0].__setitem__("strip-components", 0),
        "unknown key on an inline": lambda s: s[1].__setitem__("mode", "0755"),
        "unknown source type": lambda s: s.append({"type": "shell", "commands": ["id"]}),
        "git source type": lambda s: s.append({"type": "git", "url": "https://x/y.git", "commit": "0"}),
        "file source type": lambda s: s.append({"type": "file", "url": "https://x/y", "sha256": sha("0")}),
    }
    failed = []
    for name, f in cases.items():
        try:
            check(lock, mutate(f))
        except Bad:
            continue
        failed.append(name)
    for name, bad_lock in {
        "git dependency": {"package": [{"name": "g", "version": "1.0.0", "source": "git+https://x/y#abc"}]},
        "foreign registry": {"package": [{"name": "g", "version": "1.0.0", "source": "registry+https://x/index", "checksum": sha("a")}]},
    }.items():
        try:
            check(bad_lock, [])
        except Bad:
            continue
        failed.append(name)
    if failed:
        sys.exit("self-test FAILED, accepted: " + "; ".join(failed))
    print(f"self-test ok: 1 valid input accepted, {len(cases) + 2} tampered inputs rejected")


def main(argv):
    if argv == ["--self-test"]:
        self_test()
        return
    if len(argv) != 2:
        sys.exit("usage: flatpak-cargo-check.py LOCK SOURCES.json | --self-test")
    with open(argv[0], "rb") as f:
        lock = tomllib.load(f)
    with open(argv[1]) as f:
        sources = json.load(f)
    try:
        n = check(lock, sources)
    except Bad as e:
        sys.exit(f"error: generated sources rejected: {e}")
    print(f"verified {n} crates (archives, checksum files and cargo config) against {argv[0]}")


if __name__ == "__main__":
    main(sys.argv[1:])
