# /// script
# requires-python = ">=3.9"
# dependencies = ["bleak"]
# ///
"""GripApprover PC 常駐デーモン

ESP32 と BLE で常時接続し、hook からの HTTP リクエストを握った回数で待たせる。
起動: uv run daemon/grip_daemon.py
"""
import asyncio
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from bleak import BleakClient, BleakScanner

DEVICE_NAME = "GripApprover"
COUNT_UUID = "8f1e0002-4b1a-4c3e-9d2a-6a7b6e0c1a01"
CMD_UUID = "8f1e0003-4b1a-4c3e-9d2a-6a7b6e0c1a01"
HOST, PORT = "127.0.0.1", 8766
BAR_WIDTH = 30

cond = threading.Condition()
approval_lock = threading.Lock()
state = {
    "total": 0,          # 起動後の総握り回数
    "connected": False,
    "conn_gen": 0,       # 接続ごとに増える番号。待機中の切断を見分ける
}
ble = {"loop": None, "client": None}


def log(msg):
    sys.stdout.write("\r\x1b[2K" + msg + "\n")
    sys.stdout.flush()


def show_progress(label, done, reps, remaining):
    filled = min(BAR_WIDTH, BAR_WIDTH * done // reps) if reps else BAR_WIDTH
    bar = "#" * filled + "-" * (BAR_WIDTH - filled)
    sys.stdout.write(f"\r\x1b[2K{label} [{bar}] {done}/{reps}回 残り{remaining:.0f}秒")
    sys.stdout.flush()


def send_command(cmd):
    """ESP32 にブザーのコマンドを送る。失敗しても判定結果は変えない。"""
    loop, client = ble["loop"], ble["client"]
    if loop is None or client is None:
        return
    try:
        fut = asyncio.run_coroutine_threadsafe(
            client.write_gatt_char(CMD_UUID, cmd.encode(), response=False), loop
        )
        fut.result(timeout=2)
    except Exception as e:  # noqa: BLE001
        log(f"ブザーのコマンド送信に失敗しました: {e}")


def wait_for_grips(reps, timeout, label):
    deadline = time.monotonic() + timeout

    with cond:
        if not state["connected"]:
            return {"result": "offline", "done": 0, "reps": reps}

    # 締め切りは要求が届いた時刻から数える。順番待ちの時間も含めて timeout に収める
    if not approval_lock.acquire(timeout=max(0.0, deadline - time.monotonic())):
        return {"result": "timeout", "done": 0, "reps": reps}
    try:
        with cond:
            base = state["total"]
            gen = state["conn_gen"]
            done = 0
            while True:
                done = state["total"] - base
                remaining = deadline - time.monotonic()
                if done >= reps:
                    result = "approved"
                    break
                if not state["connected"] or state["conn_gen"] != gen:
                    result = "timeout"
                    break
                if remaining <= 0:
                    result = "timeout"
                    break
                show_progress(label, done, reps, remaining)
                cond.wait(min(0.5, remaining))
        done = min(done, reps)
        show_progress(label, done, reps, max(0.0, deadline - time.monotonic()))
        log("  → 承認" if result == "approved" else "  → 時間切れ")
        send_command("A" if result == "approved" else "D")
        return {"result": result, "done": done, "reps": reps}
    finally:
        approval_lock.release()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        url = urlparse(self.path)
        if url.path != "/wait":
            self.send_error(404)
            return
        q = parse_qs(url.query)
        try:
            reps = int(q.get("reps", ["0"])[0])
            timeout = float(q.get("timeout", ["60"])[0])
        except ValueError:
            self.send_error(400)
            return
        label = q.get("label", ["承認待ち"])[0]
        body = json.dumps(wait_for_grips(reps, timeout, label), ensure_ascii=False).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass  # アクセスログで進捗表示が崩れないようにする


def on_count(last, data):
    value = int.from_bytes(bytes(data[:4]), "little")
    # 値が減ったら ESP32 が再起動したとみなし、新しい値をそのまま加算する
    delta = value - last["value"] if value >= last["value"] else value
    last["value"] = value
    if delta:
        with cond:
            state["total"] += delta
            cond.notify_all()


def set_connected(flag):
    with cond:
        state["connected"] = flag
        if flag:
            state["conn_gen"] += 1
        cond.notify_all()


async def ble_main():
    ble["loop"] = asyncio.get_running_loop()
    while True:
        log(f"{DEVICE_NAME} を探しています…")
        device = await BleakScanner.find_device_by_name(DEVICE_NAME, timeout=10.0)
        if device is None:
            continue

        disconnected = asyncio.Event()

        def on_disconnect(_client):
            set_connected(False)
            disconnected.set()

        try:
            async with BleakClient(device, disconnected_callback=on_disconnect) as client:
                raw = await client.read_gatt_char(COUNT_UUID)
                last = {"value": int.from_bytes(bytes(raw[:4]), "little")}
                await client.start_notify(COUNT_UUID, lambda _c, data: on_count(last, data))
                ble["client"] = client
                set_connected(True)
                log("接続しました")
                await disconnected.wait()
        except Exception as e:  # noqa: BLE001
            log(f"BLE エラー: {e}")
        finally:
            ble["client"] = None
            set_connected(False)
        log("切断されました")
        await asyncio.sleep(1)


def main():
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    log(f"http://{HOST}:{PORT} で待ち受けています")
    try:
        asyncio.run(ble_main())
    except KeyboardInterrupt:
        log("終了します")
    finally:
        server.shutdown()


if __name__ == "__main__":
    main()
