#!/usr/bin/env python3
"""Private Telegram bot for publishing stable Penumbra GitHub releases.

Required environment:
  TELEGRAM_BOT_TOKEN
  TELEGRAM_CHAT_ID       private Telegram chat ID allowed to use the bot
  GITHUB_TOKEN           token with Contents: write for the target repository
Optional:
  TELEGRAM_USER_ID       additionally restrict the Telegram sender ID
  RELEASE_REPOSITORY     defaults to almightygodthor/penumbra
  RELEASE_BRANCH         defaults to main
  RELEASE_BOT_GITHUB_TOKEN  PAT used only to queue the next long-poll worker
"""

import json
import os
import re
import time
import urllib.error
import urllib.parse
import urllib.request

TG_TOKEN = os.environ["TELEGRAM_BOT_TOKEN"]
ALLOWED_CHAT = str(os.environ["TELEGRAM_CHAT_ID"])
ALLOWED_USER = str(os.environ.get("TELEGRAM_USER_ID", "")).strip()
GH_TOKEN = os.environ.get("GITHUB_TOKEN", "")
CONTINUATION_TOKEN = os.environ.get("RELEASE_BOT_GITHUB_TOKEN", "")
REPO = os.environ.get("RELEASE_REPOSITORY", "almightygodthor/penumbra")
BRANCH = os.environ.get("RELEASE_BRANCH", "main")
WORKFLOW_FILE = "stable-release-bot.yml"
TG_API = f"https://api.telegram.org/bot{TG_TOKEN}"
GH_API = "https://api.github.com"
WORKER_SECONDS = 5 * 60 * 60 + 35 * 60
POLL_TIMEOUT = 20
# Volatile conversation state is fine: a release flow must be completed in one worker window.
SESSIONS = {}


class APIError(RuntimeError):
    pass


def request_json(url, method="GET", payload=None, token=None, timeout=35):
    headers = {"User-Agent": "Penumbra-Stable-Release-Bot"}
    body = None
    if token:
        headers["Authorization"] = f"Bearer {token}"
        headers["Accept"] = "application/vnd.github+json"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        raise APIError(f"HTTP {exc.code}: {detail[:350]}") from exc
    except urllib.error.URLError as exc:
        raise APIError(f"Network error: {exc.reason}") from exc


def tg(method, payload=None, timeout=25):
    result = request_json(f"{TG_API}/{method}", "POST", payload, timeout=timeout)
    if not result.get("ok"):
        raise APIError(result.get("description", f"Telegram {method} failed"))
    return result.get("result")


def gh(method, path, payload=None, token=None):
    return request_json(
        f"{GH_API}{path}", method, payload,
        token=token or GH_TOKEN, timeout=35
    )


def send(chat_id, text, keyboard=None):
    payload = {"chat_id": chat_id, "text": text, "disable_web_page_preview": True}
    if keyboard is not None:
        payload["reply_markup"] = {"inline_keyboard": keyboard}
    return tg("sendMessage", payload)


def edit(chat_id, message_id, text, keyboard=None):
    payload = {"chat_id": chat_id, "message_id": message_id, "text": text,
               "disable_web_page_preview": True}
    if keyboard is not None:
        payload["reply_markup"] = {"inline_keyboard": keyboard}
    try:
        return tg("editMessageText", payload)
    except APIError as exc:
        if "message is not modified" in str(exc).lower():
            return None
        raise


def home_keyboard():
    return [[{"text": "🚀 Start Release", "callback_data": "release:start"}]]


def cancel_keyboard():
    return [[{"text": "✖ Cancel", "callback_data": "release:cancel"}]]


def confirm_keyboard():
    return [
        [{"text": "✅ Publish Stable Release", "callback_data": "release:publish"}],
        [{"text": "✏️ Edit Changelog", "callback_data": "release:edit"}],
        [{"text": "✖ Cancel", "callback_data": "release:cancel"}],
    ]


def allowed(chat_id, user_id):
    return str(chat_id) == ALLOWED_CHAT and (not ALLOWED_USER or str(user_id) == ALLOWED_USER)


def current_latest_release():
    try:
        return gh("GET", f"/repos/{REPO}/releases/latest")
    except APIError as exc:
        if "HTTP 404" in str(exc):
            return None
        raise


def next_stable_tag():
    latest = current_latest_release()
    if not latest:
        return "v1.0.0"
    current = str(latest.get("tag_name", ""))
    match = re.fullmatch(r"v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", current)
    if not match:
        raise APIError(
            f"Latest release tag {current!r} is not semantic X.Y.Z. "
            "Rename/tag the latest release as vMAJOR.MINOR.PATCH before publishing."
        )
    major, minor, patch = map(int, match.groups())
    return f"v{major}.{minor}.{patch + 1}"


def publish_release(chat_id, message_id, changelog):
    if not changelog.strip():
        raise APIError("Changelog cannot be empty.")
    tag = next_stable_tag()

    # Recheck immediately before publishing to reduce accidental duplicate tags.
    try:
        gh("GET", f"/repos/{REPO}/git/ref/tags/{urllib.parse.quote(tag, safe='')}")
        raise APIError(f"Tag {tag} already exists. No release was created.")
    except APIError as exc:
        if "HTTP 404" not in str(exc):
            raise

    release = gh("POST", f"/repos/{REPO}/releases", {
        "tag_name": tag,
        "target_commitish": BRANCH,
        "name": f"Penumbra {tag}",
        "body": changelog.strip(),
        "draft": False,
        "prerelease": False,
        "make_latest": "true",
    })
    SESSIONS.pop(str(chat_id), None)
    url = release.get("html_url", f"https://github.com/{REPO}/releases/tag/{tag}")
    edit(chat_id, message_id,
         f"✅ Stable release published\n\n{release.get('name', tag)}\n{url}",
         [[{"text": "🔗 Open GitHub Release", "url": url}],
          [{"text": "🚀 Start Another Release", "callback_data": "release:start"}]])
    print(f"Published {REPO} {tag}: {url}", flush=True)


def start_release(chat_id, message_id):
    SESSIONS[str(chat_id)] = {"state": "changelog", "message_id": message_id, "changelog": ""}
    edit(chat_id, message_id,
         "📝 Send the changelog as your next Telegram message.\n\n"
         "You can send multiple lines. The exact text will become the GitHub release notes.",
         cancel_keyboard())


def handle_message(message):
    chat = message.get("chat", {})
    sender = message.get("from", {})
    chat_id, user_id = chat.get("id"), sender.get("id")
    text = (message.get("text") or "").strip()
    if not allowed(chat_id, user_id):
        return
    if text.startswith("/"):
        command = text.split()[0].split("@")[0].lower()
        if command in {"/start", "/release"}:
            send(chat_id, "🚀 Penumbra Stable Release Manager\n\n"
                          "Publish a stable GitHub release with your own changelog.",
                 home_keyboard())
        elif command == "/cancel":
            SESSIONS.pop(str(chat_id), None)
            send(chat_id, "Release flow cancelled.", home_keyboard())
        elif command == "/help":
            send(chat_id, "/start — open release menu\n/release — start a release\n"
                          "/cancel — cancel the current release")
        return

    session = SESSIONS.get(str(chat_id))
    if not session or session.get("state") != "changelog":
        return
    if not text:
        send(chat_id, "Please send a non-empty text changelog.", cancel_keyboard())
        return
    session["changelog"] = text
    session["state"] = "confirm"
    try:
        tag = next_stable_tag()
    except Exception as exc:
        send(chat_id, f"Could not determine next version: {exc}\nNo release has been created.")
        SESSIONS.pop(str(chat_id), None)
        return
    # Plain text is used deliberately: user-supplied changelog is never parsed as HTML.
    preview = text[:3000] + ("…" if len(text) > 3000 else "")
    send(chat_id, f"📦 Release preview\n\nRepository: {REPO}\nTag: {tag}\n"
                   f"Target branch: {BRANCH}\nType: Stable (not a prerelease)\n\n"
                   f"Changelog:\n\n{preview}\n\nPublish this release?",
         confirm_keyboard())


def handle_callback(query):
    query_id = query.get("id")
    message = query.get("message") or {}
    chat = message.get("chat") or {}
    sender = query.get("from") or {}
    chat_id, user_id = chat.get("id"), sender.get("id")
    message_id = message.get("message_id")
    data = query.get("data", "")
    try:
        tg("answerCallbackQuery", {"callback_query_id": query_id})
    except Exception:
        pass
    if not allowed(chat_id, user_id):
        return
    try:
        if data == "release:start":
            start_release(chat_id, message_id)
        elif data == "release:cancel":
            SESSIONS.pop(str(chat_id), None)
            edit(chat_id, message_id, "Release cancelled.", home_keyboard())
        elif data == "release:edit":
            start_release(chat_id, message_id)
        elif data == "release:publish":
            session = SESSIONS.get(str(chat_id))
            if not session or session.get("state") != "confirm" or not session.get("changelog", "").strip():
                edit(chat_id, message_id, "No pending changelog. Start a new release.", home_keyboard())
                return
            edit(chat_id, message_id, "⏳ Publishing stable release…", [])
            publish_release(chat_id, message_id, session["changelog"])
    except Exception as exc:
        print(f"Callback failed: {exc}", flush=True)
        try:
            send(chat_id, f"❌ Release failed: {exc}\nNo success is reported unless GitHub confirms it.",
                 home_keyboard())
        except Exception:
            pass


def queue_next_worker():
    if not CONTINUATION_TOKEN:
        print("RELEASE_BOT_GITHUB_TOKEN is unset; worker will stop at its time limit.", flush=True)
        return
    try:
        gh("POST", f"/repos/{REPO}/actions/workflows/{WORKFLOW_FILE}/dispatches",
           {"ref": BRANCH}, token=CONTINUATION_TOKEN)
        print("Queued next Telegram worker.", flush=True)
    except Exception as exc:
        print(f"Could not queue next worker: {exc}", flush=True)


def main():
    print(f"Penumbra release bot online for {REPO}", flush=True)
    tg("deleteWebhook", {"drop_pending_updates": False})
    deadline = time.monotonic() + WORKER_SECONDS
    offset = 0
    while time.monotonic() < deadline:
        try:
            updates = tg("getUpdates", {
                "offset": offset, "timeout": POLL_TIMEOUT,
                "allowed_updates": ["message", "callback_query"],
            }, timeout=POLL_TIMEOUT + 10) or []
            for update in updates:
                try:
                    if "message" in update:
                        handle_message(update["message"])
                    elif "callback_query" in update:
                        handle_callback(update["callback_query"])
                except Exception as exc:
                    print(f"Update handling error: {exc}", flush=True)
            if updates:
                offset = max(item["update_id"] for item in updates) + 1
        except Exception as exc:
            print(f"Telegram polling error: {exc}", flush=True)
            time.sleep(3)
    queue_next_worker()


if __name__ == "__main__":
    main()
