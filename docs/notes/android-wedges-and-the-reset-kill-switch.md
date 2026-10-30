# Android MTP wedges, and why the transport reset is a kill switch there

What a day of hardware work established about wedged Android MTP sessions: how a wedge looks, what
triggers it, what recovers it, and why `DEVICE_RESET` (SIC `0x66`) is the **last** thing to try on a
phone, not the second.

Hardware: Pixel 9 Pro XL (Android), Galaxy S23 Ultra SM-S918B, both on macOS/nusb, 2026-07-20 and
2026-07-21. Related: [#18](https://github.com/vdavid/mtp-rs/issues/18),
[community-threads.md](community-threads.md), [../debugging.md](../debugging.md).

## The short version

1. The wedge is **not Samsung-specific**. A Pixel wedges from the same trigger.
2. The two device families wedge with **different signatures**, and a consumer watching only for
   `Error::DeviceReset` will never notice the Pixel case.
3. On a healthy Pixel, the transport reset **permanently breaks the MTP function** until the user
   physically replugs. It's a kill switch, not a recovery step.
4. A Pixel's dropped-future wedge **self-heals on a fresh open** (a new process), so spaced reopens
   are the whole recovery for that kind.
5. A **cancel** wedge is different, and the two families split (see the control run below). On a
   Pixel `MtpServer` exits, so only a replug helps. On a Galaxy S23 Ultra the server is alive but stuck,
   and the reset is what frees it. So `cancel_transfer` keeps its automatic reset.

## Two wedge signatures

The trigger is the same on both: an in-flight operation future dropped mid-transaction. What the host
sees afterwards isn't.

- **Galaxy S23 Ultra**: the next operation returns `Error::DeviceReset`. Loud, typed, easy to detect.
- **Pixel 9 Pro XL**: the next operation simply **hangs**. No error, no reset, nothing to match on
  (verified on a Pixel 9 Pro XL, macOS/nusb, 2026-07-20).

That difference is the practical point of this note. Detection logic built around `DeviceReset`
(which is what #18 produced, and what our docs described) is blind on the more common device class.
A consumer needs a **timeout** around every operation as well, not just an error match, and should
treat "an operation that never returns" as a wedge in its own right.

## The trigger: a dropped in-flight future

Dropping an operation's future after its command goes out but before the response is drained wedges
the device. Established by a clean A/B on a Pixel 9 Pro XL: the same binary, minutes apart, one run
without the drop finished fine, the run with the drop hung (verified on a Pixel 9 Pro XL,
macOS/nusb, 2026-07-20).

Two things that are **not** the trigger:

- **Backlog or transfer size.** `doctor --probe-cancel` reported `wedged_recovered` on a 36-byte file
  (verified on a Galaxy S23 Ultra SM-S918B, macOS/nusb, 2026-07-20).
- **Cancellation specifically.** An explicit `cancel()` is one way in, but a plain dropped future
  with no cancel reaches the same state through `recover_if_needed`'s drain.

## What the reset does to a healthy Pixel

The SIC `DEVICE_RESET` (`0x66`) was sent to a **healthy** Pixel 9 Pro XL while capturing Android's
own logs over adb (verified on a Pixel 9 Pro XL, macOS/nusb + `adb logcat`, 2026-07-21):

```
15:07:37.168  mtp-rs: sending SIC DEVICE_RESET (0x66)
15:07:38.094  W/MtpServer: got response 0x201E in command MTP_OPERATION_OPEN_SESSION (1002)
15:07:38.118  E/MtpServer: request read returned -1, errno: 125          <- ECANCELED
15:07:38.119  I/libpixelusb-UsbDataSessionMonitor: Update device state udc: configured
15:07:38.119  E/d.process.media: Mtp got error event at 0 and 1 total: Broken pipe
15:07:38.119  E/MtpServer: request read returned -1, errno: 32           <- EPIPE
```

`MtpServer` never logs again for the rest of the capture: no restart, no re-arm. The USB device
controller meanwhile still reports `configured`, which is why the phone keeps enumerating, still
shows up in `mtp-rs devices`, and answers nothing.

Mechanism: the reset cancels the outstanding FunctionFS endpoint read (`ECANCELED`), breaks the
endpoint (`EPIPE`), and Android's `MtpServer` read loop exits without re-arming. Nothing on the
phone side brings it back.

Recovery took a **physical replug**. `mtp-rs reset` didn't answer afterwards, and 10 spaced reopen
attempts over about 100 s all timed out.

## What recovers what

- **Pixel, dropped-future wedge**: a **fresh open** recovers it. It hung twice in-process, then
  answered normally once the process exited and a new one opened the device (verified on a Pixel 9
  Pro XL, macOS/nusb, 2026-07-20). So: drop everything, stay quiet, reopen with idle-spaced retries,
  and be willing to reopen from a clean state.
- **Pixel, after a reset**: nothing in software. Physical replug only.
- **Pixel, cancel wedge**: nothing in software, reset or not. Physical replug only.
- **Samsung, cancel wedge**: the reset. Spaced reopens alone didn't recover it.
- **Samsung, dropped-future wedge**: the reset-then-spaced-reopen sequence worked (transport reset,
  reopens returning `Timeout`, then `SessionAlreadyOpen`, then success). The control was never run
  for this trigger, only for a cancel wedge.
- **Samsung, dropped held-open streaming `GetObject`**: needed a physical replug (plain reopen and
  transport reset both failed).

## Control run: cancel wedges with and without the reset

The cancel test (`test_cancel_download_then_reuse_session`, a 697 KB file on the Pixel) run back to back on a
throwaway build whose only change skipped the reset in `cancel_transfer` step 5, with 10 spaced reopens after
each wedge (macOS/nusb, 2026-10-30, Pixel 9 Pro XL and Galaxy S23 Ultra SM-S918B). Asked because Galaxy A57
reports (#43) looked like the kill switch below.

- **Pixel 9 Pro XL**: 15 cancels, 3 wedges (about one in five). No reset: 2 wedges, 0 recovered (one was still
  dead after ~9 min, five of them silent). With reset: 1 wedge, 0 recovered. Replug every time.
- **Galaxy S23 Ultra**: with reset 33 cancels, 4 wedges, 4 recovered, each on the second reopen (17–18 s, no
  replug). No reset: 26 cancels, 2 wedges, 0 recovered (one still dead after ~7 min, four of them silent; a USB
  replug didn't clear it either, it took switching the USB mode off and on). The other was freed by a manual
  `mtp-rs reset` ~2 min later, and the next cancels worked.

What the phones logged (`adb logcat`, pulled after the replug):

```
Pixel, both with and without the reset, 9 ms into the cancelled GET_OBJECT:
  E/d.process.media: Mtp couldn't cancel request 42: Invalid argument
  E/MtpServer: Mtp send file got error I/O error
  W/MtpServer: [MTP] got response 0x2002 in command MTP_OPERATION_GET_OBJECT (1009)
  E/d.process.media: Mtp got error event at 0 and 1 total: Connection reset by peer
  E/MtpServer: request write returned -1, errno: 104

S23, the reset freeing a server that had been silent for ~2 min:
  E/d.process.media: Mtp error ctrlreq write data: Connection reset by peer
  E/MtpServer: request read returned -1, errno: 125
```

Mechanism on the Pixel: `MtpFfsHandle::cancelEvents` calls `io_cancel` on the in-flight requests, and one that
already finished answers `EINVAL`, which it reports as `EIO`. `MtpServer::run` only stays in its loop on `ECANCELED`,
so any other error ends it (`media/mtp/MtpServer.cpp`, `MtpFfsHandle.cpp`, AOSP `main`). A healthy cancel logs
`Operation Canceled` and a `0x201F` response instead. So on a Pixel the phone loses a race with our CLASS_CANCEL,
and no host recovery exists. The Pixel's USB debugging also vanished during that wedge; adb came back with the replug.

## Guidance that follows

For a consumer recovering a wedged Android device:

1. Drop the device and every `Storage` handle.
2. Wait a few seconds **quiet**, with no USB traffic at all.
3. Reopen with idle-spaced retries, several of them. Don't hammer close/open in a tight loop: that
   keeps the device busy and re-wedges it into a hard `Timeout`.
4. Only if spaced reopens have all failed, consider the transport reset, accepting that on Android
   it can take the MTP function down until the user replugs.

The reset is still the right tool for a device that's **already** unreachable, where you can't make
things much worse, and for the cross-process poison it was written for (a host that died mid-transfer
leaving stale bulk data). It's the wrong first move on a device that might still recover on its own.
That's why `cancel_transfer` sends it only after detecting a wedge: by then a Pixel is already gone, so it
costs nothing, and a Samsung needs it. Don't remove it.

## Confidence: which claims rest on one observation

Flagged so nobody treats them as settled:

- **Single observation**: the Pixel reset kill-switch (one healthy-device reset, one logcat capture,
  one replug recovery). The mechanism reading is well supported by the logs, but "always permanent"
  is not established, and other Android builds may re-arm `MtpServer` where this one didn't.
- **Single observation**: the Pixel wedge self-healing on a fresh open.
- **Small sample**: the S23 control (4 of 4 recovered with the reset, 0 of 2 without, plus one manual reset). The
  direction is clear, the rates aren't. Other Samsungs differ: an A57 stayed stuck after the reset (#43).
- **Solid**: the two signatures differing, and the dropped future being the trigger (clean A/B).

## Trap: a lingering adb server looks exactly like a wedge

An attached `adb` server holds the USB device, so a perfectly healthy phone presents the identical
symptom: it enumerates, `mtp-rs devices` lists it, and every open times out. This cost hours during
this investigation before it was spotted.

Before diagnosing a wedge, make sure nothing else owns the interface: no `adb` server running against
the phone, and on macOS no `ptpcamerad` (see [../debugging.md](../debugging.md), which has the
blocker loop). If a "wedged" device recovers the moment you stop `adb`, it was never wedged.
