#!/usr/bin/env python3
"""Test sender for the TunnelRez TV receiver (PROTOCOL.md v0).

Streams a pre-encoded clip (Annex-B H.264/HEVC, or AV1 low-overhead OBU stream) at a fixed frame rate,
looping, answers IDR_REQUEST by jumping to the clip start (a keyframe), sends PING every 500 ms and prints
the TV's CAPS (decoder list) and STATS. Summary at the end.

  python send_test.py <tv-ip> clips/test.h264 --codec h264 --seconds 65
  python send_test.py <tv-ip> clips/test.hevc --codec hevc
  python send_test.py <tv-ip> clips/test.obu  --codec av1

Make clips (NVENC), e.g.:
  ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=60 -t 30 -c:v h264_nvenc -preset p1 -tune ull -bf 0 -g 600 \
         -rc cbr -b:v 30M -bufsize 500k -zerolatency 1 -bsf:v h264_mp4toannexb -f h264 clips/test.h264
  (hevc_nvenc ... -bsf:v hevc_mp4toannexb -f hevc clips/test.hevc ; av1_nvenc ... -f obu clips/test.obu)
"""
import argparse
import json
import socket
import struct
import sys
import threading
import time

MAGIC = b"LRT0"
HELLO, CONFIG, FRAME, PING, PONG, STATS, IDR_REQUEST, BYE, CAPS = range(9)


def now_us():
    return time.perf_counter_ns() // 1000


def msg(typ, payload=b"", flags=0, t_us=None):
    if t_us is None:
        t_us = now_us()
    return struct.pack("<4sBBHQI", MAGIC, typ, flags, 0, t_us, len(payload)) + payload


# ---------------------------------------------------------------- splitting
def nal_units(data):
    """Yield NAL payloads (without start code) from an Annex-B stream."""
    starts = []
    i = data.find(b"\x00\x00\x01")
    while i >= 0:
        starts.append(i + 3)
        i = data.find(b"\x00\x00\x01", i + 3)
    for k, s in enumerate(starts):
        e = starts[k + 1] - 3 if k + 1 < len(starts) else len(data)
        # trailing zero of a 4-byte start code belongs to the next one
        while e > s and data[e - 1] == 0:
            e -= 1
        yield data[s:e]


def split_annexb(data, hevc):
    """Return list of (is_key, config_bytes_or_None, frame_bytes) access units.
    Parameter sets go to CONFIG (not into the frame); SEI stays with the picture that follows it."""
    sc = b"\x00\x00\x00\x01"
    aus = []
    cur, cur_key, cur_vcl, cfg = [], False, False, []

    def flush():
        nonlocal cur, cur_key, cur_vcl
        if cur_vcl:
            aus.append((cur_key, b"".join(cfg) if cfg else None, b"".join(cur)))
            cfg.clear()
        cur, cur_key, cur_vcl = [], False, False

    for nal in nal_units(data):
        if not nal:
            continue
        if hevc:
            t = (nal[0] >> 1) & 0x3F
            is_ps, is_aud, is_vcl = t in (32, 33, 34), t == 35, t < 32
            is_key = 16 <= t <= 21
            first = is_vcl and len(nal) > 2 and bool(nal[2] & 0x80)  # first_slice_segment_in_pic_flag
        else:
            t = nal[0] & 0x1F
            is_ps, is_aud, is_vcl = t in (7, 8), t == 9, 1 <= t <= 5
            is_key = t == 5
            first = is_vcl and len(nal) > 1 and bool(nal[1] & 0x80)  # first_mb_in_slice == 0
        if is_aud:
            continue
        if cur_vcl and (not is_vcl or first):
            flush()
        if is_ps:
            cfg.append(sc + nal)
            continue
        cur.append(sc + nal)
        cur_vcl = cur_vcl or is_vcl
        cur_key = cur_key or is_key
    flush()
    # NVENC emits parameter sets only with the first IDR: resend them with every keyframe.
    last_cfg, out = None, []
    for k, c, f in aus:
        if c:
            last_cfg = c
        out.append((k, c or (last_cfg if k else None), f))
    return out


def obus(data):
    i = 0
    while i < len(data):
        h = data[i]
        t = (h >> 3) & 0xF
        ext = (h >> 2) & 1
        p = i + 1 + ext
        size, shift = 0, 0
        while True:
            b = data[p]
            p += 1
            size |= (b & 0x7F) << shift
            shift += 7
            if not b & 0x80:
                break
        end = p + size
        yield t, data[i:end]
        i = end


def split_av1(data, keep_seq):
    """TUs start at temporal delimiters. Keyframe TUs carry the sequence header (NVENC)."""
    aus = []
    cur, seq = [], None
    last_seq = None
    for t, ob in obus(data):
        if t == 2 and cur:
            aus.append((seq is not None, seq, b"".join(cur)))
            cur, seq = [], None
        if t == 1:
            seq = ob
            last_seq = ob
            if not keep_seq:
                continue
        cur.append(ob)
    if cur:
        aus.append((seq is not None, seq, b"".join(cur)))
    return aus


# ---------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("file")
    ap.add_argument("--codec", choices=["h264", "hevc", "av1"], required=True)
    ap.add_argument("--port", type=int, default=7878)
    ap.add_argument("--fps", type=float, default=60.0)
    ap.add_argument("--seconds", type=float, default=65.0)
    ap.add_argument("--width", type=int, default=1920)
    ap.add_argument("--height", type=int, default=1080)
    ap.add_argument("--keep-seq-in-frame", action="store_true", help="AV1: leave the sequence header in keyframe TUs")
    ap.add_argument("--warmup", type=float, default=3.0, help="seconds excluded from the summary")
    ap.add_argument("--log", help="write every STATS line here (JSONL)")
    a = ap.parse_args()

    data = open(a.file, "rb").read()
    if a.codec == "av1":
        aus = split_av1(data, a.keep_seq_in_frame)
    else:
        aus = split_annexb(data, a.codec == "hevc")
    keys = sum(1 for k, _, _ in aus if k)
    print(f"{a.file}: {len(aus)} access units, {keys} keyframes, first is key: {aus[0][0]}", flush=True)
    assert aus[0][0], "clip must start with a keyframe"

    s = socket.create_connection((a.host, a.port), timeout=5)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 2 * 1024 * 1024)
    s.settimeout(None)
    lock = threading.Lock()

    def send(b):
        with lock:
            s.sendall(b)

    state = {"idr": False, "stop": False, "rtt": [], "stats": [], "idr_count": 0, "clock": None}
    t0 = time.perf_counter()
    logf = open(a.log, "w") if a.log else None

    def reader():
        buf = b""
        try:
            while not state["stop"]:
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
                while len(buf) >= 20:
                    magic, typ, flags, _, t_us, ln = struct.unpack("<4sBBHQI", buf[:20])
                    if magic != MAGIC:
                        print("bad magic from TV", file=sys.stderr)
                        return
                    if len(buf) < 20 + ln:
                        break
                    p, buf = buf[20 : 20 + ln], buf[20 + ln :]
                    if typ == PONG:
                        echo, tv = struct.unpack("<QQ", p)
                        back = now_us()
                        rtt = back - echo
                        state["rtt"].append(rtt / 1000.0)
                        # TV clock - sender clock, from the min-RTT sample
                        if state["clock"] is None or rtt < state["clock"][0]:
                            state["clock"] = (rtt, tv - (echo + back) // 2)
                    elif typ == STATS:
                        st = json.loads(p)
                        st["_t"] = round(time.perf_counter() - t0, 2)
                        if "disp_us" in st and state["clock"] is not None:
                            st["_e2e_ms"] = round((st["disp_us"] - state["clock"][1] - st["disp_t_us"]) / 1000.0, 1)
                        state["stats"].append(st)
                        if logf:
                            logf.write(json.dumps(st) + "\n")
                        print("STATS", json.dumps(st), flush=True)
                    elif typ == CAPS:
                        caps = json.loads(p)
                        for d in caps.get("decoders", []):
                            print(f"CAPS {d['mime']:<14} {d['name']:<34} max {d['max_w']}x{d['max_h']}@{d['max_fps']} "
                                  f"{d['max_bps'] / 1e6:.0f} Mb/s hw={d.get('hw')} perf={d.get('perf', [])}", flush=True)
                    elif typ == IDR_REQUEST:
                        state["idr"] = True
                        state["idr_count"] += 1
                        print(f"IDR_REQUEST at {time.perf_counter() - t0:.2f}s", flush=True)
        except OSError:
            pass

    threading.Thread(target=reader, daemon=True).start()
    hello = {"v": 0, "codec": a.codec, "width": a.width, "height": a.height, "fps": int(a.fps)}
    send(msg(HELLO, json.dumps(hello).encode()))

    period = 1.0 / a.fps
    n_frames = int(a.seconds * a.fps)
    i = 0
    next_t = time.perf_counter()
    last_ping = 0.0
    sent_bytes = 0
    try:
        for n in range(n_frames):
            if state["idr"]:
                state["idr"] = False
                i = 0  # clip start is a keyframe
            key, cfg, frame = aus[i]
            t = now_us()
            if key and cfg:
                send(msg(CONFIG, cfg, t_us=t))
            send(msg(FRAME, frame, flags=1 if key else 0, t_us=t))
            sent_bytes += len(frame)
            i = (i + 1) % len(aus)
            nowp = time.perf_counter()
            if nowp - last_ping >= 0.5:
                send(msg(PING, struct.pack("<Q", now_us())))
                last_ping = nowp
            next_t += period
            d = next_t - time.perf_counter()
            if d > 0:
                time.sleep(d) if d > 0.002 else None
                while time.perf_counter() < next_t:
                    pass
            elif d < -0.1:
                next_t = time.perf_counter()  # fell behind, resync
        send(msg(BYE))
    finally:
        time.sleep(1.2)
        state["stop"] = True
        s.close()

    el = time.perf_counter() - t0
    st = [x for x in state["stats"] if x["_t"] >= a.warmup]
    print(f"\nsent {n_frames} frames in {el:.1f}s, {sent_bytes * 8 / el / 1e6:.1f} Mb/s, IDR_REQUESTs: {state['idr_count']}")
    if st:
        avg = lambda k: sum(x[k] for x in st) / len(st)
        mx = lambda k: max(x[k] for x in st)
        rtt = sorted(state["rtt"])
        print(f"summary over {len(st)} s (after {a.warmup:.0f}s warm-up): codec={st[-1]['codec']}")
        print(f"  fps_in avg {avg('fps_in'):.2f}  fps_out avg {avg('fps_out'):.2f} min {min(x['fps_out'] for x in st):.1f}")
        print(f"  decode_ms p50 avg {avg('decode_ms_p50'):.2f}  p99 avg {avg('decode_ms_p99'):.2f} max {mx('decode_ms_p99'):.1f}")
        print(f"  dropped (cumulative) {st[-1]['dropped']}  late {st[-1]['late']}  queue max {mx('queue')}")
        print(f"  recv_kbps avg {avg('recv_kbps'):.0f}")
        if rtt:
            print(f"  ping rtt ms p50 {rtt[len(rtt) // 2]:.2f}  max {rtt[-1]:.2f}")


if __name__ == "__main__":
    main()
