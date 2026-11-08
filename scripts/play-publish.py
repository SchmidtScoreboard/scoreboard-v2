#!/usr/bin/env python3
"""Publish the Android app to Google Play through the Play Developer API.

    scripts/play-publish.py                  what each track holds (read-only)
    scripts/play-publish.py upload [--track internal] [--aab PATH]
                            [--notes TEXT | --notes-file PATH] [--draft] [--dry-run]
    scripts/play-publish.py listing [--dry-run]

listing replaces the en-US store listing with what the repo holds: the text in
design/store/play/ (title, short and full description), the icon
design/icon/play-icon.png (made by design/icon/gen_icon.py), and the feature
graphic and phone screenshots design/store/compose.py writes to
design/store/out/.

Build the bundle first: `./gradlew bundleRelease` in android/ signs it with the
upload key (see android/app/build.gradle.kts). Every versionCode Play receives
is spent for good, so bump it before each upload.

Auth is a service account key, ~/.config/scoreboard/play-service-account.json
or wherever $SCOREBOARD_PLAY_SERVICE_ACCOUNT points, invited as a user in Play
Console with access to the app. NEVER commit it. Stdlib only: the token is
signed with the system openssl, so there is nothing to install.

Everything happens inside one Play "edit", which is invisible until it
commits. A failed step deletes the edit, so a half-done upload never lands;
--dry-run runs every step (upload included) and then deletes it on purpose.

Track names: internal, alpha (Closed testing), beta (Open testing),
production. An app that has never been published only accepts draft
releases; upload falls back to one and says so, and the rollout is then
finished in Play Console.
"""

import argparse
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

PACKAGE = "com.markschmidt.openscoreboard"
KEY = os.environ.get(
    "SCOREBOARD_PLAY_SERVICE_ACCOUNT",
    os.path.expanduser("~/.config/scoreboard/play-service-account.json"),
)
AAB = os.path.expanduser(
    "~/Library/Caches/scoreboard-android/app/outputs/bundle/release/app-release.aab"
)
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LANG = "en-US"
# Play's limits on the listing text.
TEXT_LIMITS = {"title": 30, "short-description": 80, "full-description": 4000}
API = f"https://androidpublisher.googleapis.com/androidpublisher/v3/applications/{PACKAGE}"
UPLOAD = f"https://androidpublisher.googleapis.com/upload/androidpublisher/v3/applications/{PACKAGE}"


class PlayError(Exception):
    pass


def b64url(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def sign_rs256(key_pem, data):
    # The key reaches openssl through a pipe, never a file on disk.
    r, w = os.pipe()
    os.write(w, key_pem.encode())
    os.close(w)
    try:
        return subprocess.run(
            ["openssl", "dgst", "-sha256", "-sign", f"/dev/fd/{r}"],
            input=data, capture_output=True, pass_fds=(r,), check=True,
        ).stdout
    finally:
        os.close(r)


def access_token():
    try:
        with open(KEY) as f:
            sa = json.load(f)
    except FileNotFoundError:
        sys.exit(f"no service account key at {KEY}")
    now = int(time.time())
    header = b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    claims = b64url(json.dumps({
        "iss": sa["client_email"],
        "scope": "https://www.googleapis.com/auth/androidpublisher",
        "aud": "https://oauth2.googleapis.com/token",
        "iat": now,
        "exp": now + 3600,
    }).encode())
    unsigned = f"{header}.{claims}".encode()
    jwt = unsigned.decode() + "." + b64url(sign_rs256(sa["private_key"], unsigned))
    body = urllib.parse.urlencode({
        "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
        "assertion": jwt,
    }).encode()
    return call("POST", "https://oauth2.googleapis.com/token", data=body,
                ctype="application/x-www-form-urlencoded")["access_token"]


def call(method, url, token=None, body=None, data=None, ctype="application/json"):
    if body is not None:
        data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", ctype)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            raw = resp.read()
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        try:
            detail = json.loads(detail)["error"]["message"]
        except (ValueError, KeyError, TypeError):
            pass
        raise PlayError(f"{method} {url.split(PACKAGE)[-1] or url}: HTTP {e.code}: {detail}")
    return json.loads(raw) if raw.strip() else {}


class Edit:
    """One Play edit: deleted on the way out unless it was committed."""

    def __init__(self, token):
        self.token = token
        self.id = None
        self.committed = False

    def __enter__(self):
        self.id = call("POST", f"{API}/edits", self.token, body={})["id"]
        return self

    def __exit__(self, *exc):
        if self.id and not self.committed:
            try:
                call("DELETE", f"{API}/edits/{self.id}", self.token)
            except PlayError:
                pass  # an edit Play already discarded is just as gone
        return False

    def get(self, path):
        return call("GET", f"{API}/edits/{self.id}{path}", self.token)

    def put(self, path, body):
        return call("PUT", f"{API}/edits/{self.id}{path}", self.token, body=body)

    def post(self, path):
        return call("POST", f"{API}/edits/{self.id}{path}", self.token, body={})

    def upload_bundle(self, path):
        with open(path, "rb") as f:
            data = f.read()
        return call("POST", f"{UPLOAD}/edits/{self.id}/bundles?uploadType=media",
                    self.token, data=data, ctype="application/octet-stream")

    def replace_images(self, kind, paths):
        call("DELETE", f"{API}/edits/{self.id}/listings/{LANG}/{kind}", self.token)
        for path in paths:
            with open(path, "rb") as f:
                data = f.read()
            call("POST", f"{UPLOAD}/edits/{self.id}/listings/{LANG}/{kind}?uploadType=media",
                 self.token, data=data, ctype="image/png")

    def commit(self):
        self.post(":commit")
        self.committed = True


def status(_args):
    with Edit(access_token()) as e:
        details = e.get("/details")
        bundles = e.get("/bundles").get("bundles", [])
        tracks = e.get("/tracks").get("tracks", [])
    print(f"{PACKAGE}  (default language {details.get('defaultLanguage', '?')})")
    codes = sorted(b["versionCode"] for b in bundles)
    print(f"bundles uploaded: {', '.join(map(str, codes)) or 'none'}")
    for t in tracks:
        releases = t.get("releases", [])
        if not releases:
            print(f"  {t['track']:<12} (empty)")
        for r in releases:
            frac = f" {r['userFraction']:.0%}" if "userFraction" in r else ""
            vcs = ",".join(r.get("versionCodes", [])) or "-"
            print(f"  {t['track']:<12} {r.get('status', '?')}{frac}  "
                  f"versionCode {vcs}  {r.get('name', '')}")


def upload(args):
    if not os.path.isfile(args.aab):
        sys.exit(f"no bundle at {args.aab} (run ./gradlew bundleRelease in android/)")
    notes = args.notes
    if args.notes_file:
        with open(args.notes_file) as f:
            notes = f.read().strip()
    if notes and len(notes) > 500:
        sys.exit(f"release notes are {len(notes)} characters; Play allows 500")

    with Edit(access_token()) as e:
        size = os.path.getsize(args.aab) / 1e6
        print(f"uploading {args.aab} ({size:.1f} MB)…")
        bundle = e.upload_bundle(args.aab)
        vc = bundle["versionCode"]
        print(f"  versionCode {vc}, sha256 {bundle.get('sha256', '?')[:16]}…")

        def release(state):
            r = {"versionCodes": [str(vc)], "status": state}
            if notes:
                r["releaseNotes"] = [{"language": "en-US", "text": notes}]
            return {"track": args.track, "releases": [r]}

        state = "draft" if args.draft else "completed"
        try:
            e.put(f"/tracks/{args.track}", release(state))
            e.post(":validate")
        except PlayError as err:
            if state == "draft" or "draft" not in str(err).lower():
                raise
            # A never-published app takes draft releases only.
            print("  Play accepts only draft releases until the app is published;"
                  " uploading as a draft")
            state = "draft"
            e.put(f"/tracks/{args.track}", release(state))
            e.post(":validate")

        if args.dry_run:
            print(f"dry run: validated versionCode {vc} as a {state} release on "
                  f"'{args.track}'; discarding the edit, nothing was published")
            return
        e.commit()
    print(f"committed: versionCode {vc} is a {state} release on '{args.track}'")
    if state == "draft":
        print("  finish the rollout in Play Console (Test and release › the track)")


def listing(args):
    text = {}
    for name, limit in TEXT_LIMITS.items():
        path = os.path.join(ROOT, "design", "store", "play", f"{name}.txt")
        with open(path) as f:
            text[name] = f.read().strip()
        if not text[name] or len(text[name]) > limit:
            sys.exit(f"{os.path.relpath(path, ROOT)} is {len(text[name])} characters; Play allows 1-{limit}")
    out = os.path.join(ROOT, "design", "store", "out")
    phone = sorted(
        os.path.join(out, "play-phone", f)
        for f in os.listdir(os.path.join(out, "play-phone")) if f.endswith(".png")
    )
    images = {
        "icon": [os.path.join(ROOT, "design", "icon", "play-icon.png")],
        "featureGraphic": [os.path.join(out, "play-feature.png")],
        "phoneScreenshots": phone,
    }
    if not 2 <= len(phone) <= 8:
        sys.exit(f"Play needs 2-8 phone screenshots; design/store/out/play-phone has {len(phone)}")
    for paths in images.values():
        for path in paths:
            if not os.path.isfile(path):
                sys.exit(f"missing {os.path.relpath(path, ROOT)}")

    with Edit(access_token()) as e:
        e.put(f"/listings/{LANG}", {
            "language": LANG,
            "title": text["title"],
            "shortDescription": text["short-description"],
            "fullDescription": text["full-description"],
        })
        print(f"text: title, short ({len(text['short-description'])}), "
              f"full ({len(text['full-description'])} characters)")
        for kind, paths in images.items():
            e.replace_images(kind, paths)
            print(f"{kind}: {', '.join(os.path.relpath(p, ROOT) for p in paths)}")
        e.post(":validate")
        if args.dry_run:
            print("dry run: the listing validated; discarding the edit, nothing changed")
            return
        e.commit()
    print(f"committed the {LANG} store listing")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd")
    sub.add_parser("status", help="what each track holds (the default)")
    u = sub.add_parser("upload", help="upload a bundle and release it to a track")
    u.add_argument("--track", default="internal")
    u.add_argument("--aab", default=AAB)
    n = u.add_mutually_exclusive_group()
    n.add_argument("--notes", help="release notes, en-US (500 characters max)")
    n.add_argument("--notes-file")
    u.add_argument("--draft", action="store_true", help="create the release as a draft")
    u.add_argument("--dry-run", action="store_true",
                   help="run every step, then discard the edit instead of committing")
    lst = sub.add_parser("listing", help="replace the store listing with the repo's")
    lst.add_argument("--dry-run", action="store_true",
                     help="upload and validate everything, then discard the edit")
    args = p.parse_args()
    try:
        {"upload": upload, "listing": listing}.get(args.cmd, status)(args)
    except PlayError as err:
        sys.exit(str(err))


if __name__ == "__main__":
    main()
