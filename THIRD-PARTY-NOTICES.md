# Third-party notices

mhworld-dlss is licensed under GPL-3.0-only (see `LICENSE`). This file lists
what it is built from, what it talks to, and under which terms. It is a
plain-language summary, not legal advice.

## 1. Code compiled into the released binaries

`mhw-dlss.dll` and `mhw-dlss-launcher.exe` are built from this repository
and from the Rust crates pinned in `Cargo.lock`. Every one of those crates is
offered under "MIT OR Apache-2.0"; this project uses them under the MIT
licence, whose text is at the end of this section.

Linked into the binaries:

| crate | version | copyright |
| --- | --- | --- |
| `windows`, `windows-core`, `windows-result`, `windows-strings`, `windows-targets`, `windows_x86_64_gnu`, `windows-link` | 0.58.0, 0.58.0, 0.2.0, 0.1.0, 0.52.6, 0.52.6, 0.2.1 | Copyright (c) Microsoft Corporation |
| `parking_lot`, `parking_lot_core`, `lock_api` | 0.12.5, 0.9.12, 0.4.14 | Copyright (c) 2016 The Rust Project Developers |
| `smallvec` | 1.16.2 | Copyright (c) 2018 The Servo Project Developers |
| `scopeguard` | 1.2.0 | Copyright (c) 2016-2019 Ulrik Sverdrup "bluss" and scopeguard developers |
| `cfg-if` | 1.0.5 | Copyright (c) 2014 Alex Crichton |

Used only while building (procedural macros; none of their code is in the
binaries): `windows-implement` and `windows-interface` 0.58.0 (Microsoft
Corporation), `proc-macro2` 1.0.107, `quote` 1.0.47 and `syn` 2.0.119 (their
authors; MIT OR Apache-2.0), and `unicode-ident` 1.0.26 ((MIT OR Apache-2.0)
AND Unicode-3.0).

Listed in `Cargo.lock` but not built for 64-bit Windows: `bitflags`, `libc`,
`redox_syscall`, and the `windows_*` crates for other processors and
toolchains.

The binaries also contain the Rust standard library (MIT OR Apache-2.0,
Copyright (c) The Rust Project Contributors) and start-up code from the
MinGW-w64 runtime that the GNU Windows toolchain links in (public domain and
the Zope Public License 2.1; see mingw-w64's `COPYING.MinGW-w64-runtime.txt`).

`src/fasthash.rs` uses the finalising step of MurmurHash3, which its author,
Austin Appleby, placed in the public domain.

### The MIT licence

```text
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## 2. Interfaces declared in this source

No source code of NVIDIA, ReShade, RenoDX or OptiScaler is copied into this
repository. To call their binaries the source declares, by hand, the layouts
and names those binaries expect:

* **NVIDIA Streamline.** Structure layouts, GUIDs and version numbers in
  `src/streamline.rs` follow the public headers of the Streamline SDK 2.14.1,
  which NVIDIA publishes under the MIT licence: Copyright (c) 2023 NVIDIA
  CORPORATION. All rights reserved. The licence text is the one above.
* **NVIDIA NGX (DLSS).** Parameter names and function signatures follow the
  public headers of NVIDIA's DLSS SDK. No NVIDIA source or binary is included.
* **ReShade.** The add-on export names and the overlay function table used by
  `src/reshade.rs` and `src/overlay.rs` follow ReShade's public add-on
  headers at version 6.8.0. ReShade is Copyright 2014 Patrick Mours and is
  published under the BSD 3-Clause licence.

## 3. NVIDIA's libraries, which you download yourself

This project does not contain or distribute any NVIDIA binary: not in the
repository and not in a release archive. `fetch-nvidia.ps1` downloads
NVIDIA's own Streamline SDK 2.14.1 archive from
<https://github.com/NVIDIA-RTX/Streamline/releases>, verifies it, and copies
seven libraries from it into the `mhw-dlss` folder inside your game folder,
with the three licence texts that cover them:

| files | licence, as shipped in NVIDIA's archive |
| --- | --- |
| `nvngx_dlss.dll`, `nvngx_dlssg.dll` | NVIDIA RTX SDKs License (`nvngx_dlss.license.txt`) |
| `sl.interposer.dll`, `sl.common.dll`, `sl.pcl.dll` | MIT, as builds of the Streamline source (`streamline.license.txt`) |
| `sl.reflex.dll` | NVIDIA's SDK licence with the Reflex supplement (`reflex.license.txt`) |
| `sl.dlss_g.dll` | provided by NVIDIA as a prebuilt binary only; this project treats it like the two `nvngx` files |

The NVIDIA RTX SDKs License allows the files to be installed and used on
systems with NVIDIA GPUs, and does not allow them to be distributed as a
stand-alone product or in a way that would put them under an open source
licence. That is why a GPL-3.0 project cannot ship them and you fetch them
from NVIDIA directly. Running the fetch script means you accept NVIDIA's
terms for those files. Do not redistribute that `mhw-dlss` folder.

## 4. Optional tools this project works beside

None of these is included, required or distributed by this project: not in
the repository and not in a release archive. Each is downloaded on request,
from its authors, either by you or by `install.ps1` when you answer yes to
its question, and each is under its own licence. When `install.ps1` installs
one it writes these names and links into `COMPANION-LICENCES.txt` beside it.

| tool | downloaded from | by | licence |
| --- | --- | --- | --- |
| ReShade 6.8.0, with add-on support | <https://reshade.me> (`ReShade64.dll`, taken out of the official setup program) | crosire | [BSD 3-Clause](https://github.com/crosire/reshade/blob/main/LICENSE.md) |
| RenoDX add-on for this game | the snapshot release of <https://github.com/clshortfuse/renodx> | clshortfuse | [MIT](https://github.com/clshortfuse/renodx/blob/main/LICENSE) |
| OptiScaler 0.9.4 | the v0.9.4 release of <https://github.com/optiscaler/OptiScaler> (`OptiScaler.dll` and `OptiScaler.ini` only) | the OptiScaler project | [GPL-3.0](https://github.com/optiscaler/OptiScaler/blob/master/LICENSE) |

To open OptiScaler's `.7z` archive, `install.ps1` downloads `7zr.exe` 26.04
from 7-Zip's own repository, <https://github.com/ip7z/7zip>, into the temp
folder, uses it once and deletes it. 7-Zip is by Igor Pavlov, under the GNU
LGPL with further terms for parts of it; see <https://www.7-zip.org/license.txt>.

## 5. Trademarks

NVIDIA, GeForce, RTX, DLSS, Reflex and Streamline are trademarks of NVIDIA
Corporation. Monster Hunter: World is a trademark of CAPCOM CO., LTD. This
project is not affiliated with, made by or endorsed by NVIDIA or Capcom.
