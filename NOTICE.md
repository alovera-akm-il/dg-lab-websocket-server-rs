# Notice and attribution

This project is a Rust port of [`dglab-websocket-server`](https://github.com/dungeonlab-open/dglab-websocket-server)
(`v3-server.ts` / `v4-server.ts`), the reference WebSocket relay server for
the [DGLAB KIT](https://github.com/dungeonlab-open/dglab-kit) SDK, published
by [dungeonlab-open](https://github.com/dungeonlab-open) (DG-LAB / Dungeon
Lab). It reimplements the same wire protocol, message shapes, and error/close
codes as that original server — see the top-level [README](README.md) for
exactly how this port relates to it, and [`docs/api.md`](docs/api.md) for the
full protocol reference.

Upstream open-source repositories this port is derived from or draws
protocol/data from:

- [`dglab-websocket-server`](https://github.com/dungeonlab-open/dglab-websocket-server) — the relay server this project ports.
- [`dglab-kit`](https://github.com/dungeonlab-open/dglab-kit) — the DG-LAB SDK; source of the bundled pulse waveform presets (`src/panel/presets.rs`) and the V4 `device.op` RPC schema this port's V4 command builders (`src/panel/v4_commands.rs`) implement.
- [`dglab-websocket-simple`](https://github.com/dungeonlab-open/dglab-websocket-simple) — an earlier, simpler reference implementation of the same WebSocket protocol.
- [`dglab-bluetooth-protocol`](https://github.com/dungeonlab-open/dglab-bluetooth-protocol) — the underlying Bluetooth protocol between the DG-LAB APP and the physical device (not used directly by this project, which only ever talks to the APP over the WebSocket relay — included here for completeness/attribution).

`dglab-websocket-server` and `dglab-kit` are themselves published under the
GNU General Public License v3.0. This port is licensed under the same terms
— see [`LICENSE`](LICENSE).

## Original non-commercial-use notice (translated)

The upstream `dungeonlab-open` organization publishes the following notice
across its open-source protocol repositories (its own org profile, and the
READMEs of `dglab-bluetooth-protocol` and `dglab-websocket-simple`). It is
reproduced here, translated from the original Chinese, for attribution —
this is the closest thing to a "disclaimer" that exists in the upstream
source; no medical, health, or safety-specific disclaimer was found in any
upstream repository, org profile, or SDK documentation as of this writing
(2026-08-23). See the "Safety and medical disclaimer" section of the
top-level [README](README.md#safety-and-medical-disclaimer) for a disclaimer
written specifically for this port, which upstream does not provide.

> DG-LAB devices have been recognized and loved by many friends around the
> world. Many of you have hoped our devices could take part in more fun,
> free, and diverse scenarios. For that reason, we are sharing the protocol
> for DG-LAB's flagship device as open source. Based on this protocol, you
> can use whatever programming language, development framework, or creative
> approach you like to bring DG-LAB devices into your own entertainment
> scenarios.
>
> This open-source protocol is intended to let DG-LAB enthusiasts use their
> devices more freely. Please do not use this content for any commercial
> purpose without authorization. For commercial cooperation inquiries,
> please [contact us](https://www.dungeon-lab.com). Technical inquiries via
> QQ: 3849540080 (for open-source technical questions only).

Original Chinese (from the `dungeonlab-open` org profile and
`dglab-websocket-simple`'s README; `dglab-bluetooth-protocol`'s README
carries the same clause with "蓝牙协议" (Bluetooth protocol) in place of
"WebSocket 协议" (WebSocket protocol)):

> DG-LAB 设备在全球范围内得到了广大朋友的认可与喜爱。许多朋友希望我们的设备能够
> 参与到更多有趣、自由、多样化的场景中。因此，我们将 DG-LAB 具有代表性的设备协议
> 以开源形式分享出来。您可以基于该协议，使用不同的编程语言、开发框架或创意方案，
> 将 DG-LAB 设备接入到自己的娱乐场景中。
>
> 本开源协议旨在让 DG-LAB 爱好者更加自由地使用设备。未经授权，请勿将本内容用于
> 任何商业用途。如有商业合作需求，请[联系我们](https://www.dungeon-lab.com)。
> 技术咨询 QQ：3849540080（仅供开源技术相关问题咨询）

This project follows that same restriction: it is shared for enthusiasts to
use and modify freely, not for unauthorized commercial use. This does not
change or add to the terms of the GPLv3 license itself (see
[`LICENSE`](LICENSE)) — it is reproduced here as attribution to, and in the
spirit of, the upstream project this port builds on.
