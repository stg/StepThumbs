# StepThumbs

Fast 3D thumbnails for Windows Explorer, ready before you need them.

I've used F3D for a long time and truly appreciate it, but the time has come for
an upgrade.

![STEP thumbnails](assets/thumbnails.png)

- **About 5× faster on the tested assembly.** My 6.7 MB STEP assembly took about
  a second with the modified Look renderer, against 5–6 seconds with F3D.
  A cached thumbnail returned in 16 ms.
- **Preflight means previews are ready before you browse.** After the first scan,
  it watches for changed models and renders them at low priority; an Explorer
  cache miss gets foreground priority.
- **Windows clearing its cache doesn't mean starting over.** StepThumbs keeps
  its own disk cache, which Explorer's thumbnail provider checks first.
- **Low RAM. Zero CPU/GPU work while idle.** About 31 MiB of idle RAM in local
  tests, with the app sleeping between jobs and releasing GPU resources.
  No Windows service or periodic rescans; filesystem notifications and Explorer
  thumbnail requests wake it when needed.
- **STEP, STP, STL and GLB.** Render with source colours, edges, ambient occlusion
  and transparent or coloured backgrounds; try changes in the live preview
  using the supplied model or your own.
- **A quiet 3D tray icon.** Open activity, cache usage and settings from the tray,
  choose folders and file types, and cap model size and background workers.

## Install

1. Download the [installer from Releases](../../releases/latest) and run it.
2. Open **StepThumbs** from the Windows Start menu. Its **3D** icon appears in the
   tray; check the hidden-icons arrow if you don't see it.
3. Right-click the icon and choose **Configure** to open **File types**.
4. Tick the extensions you want, click **Install selected preview hooks**, then
   choose **Yes** to replace the listed providers.

   Optionally select **Start with Windows** under **Settings** and click
   **Save settings**.
5. Use **Large icons** or **Extra large icons** in Explorer to see the thumbnails.

<img src="assets/file-types.png" width="640" alt="StepThumbs File types settings with extension checkboxes, provider status and the install button">

Uninstall through Windows Settings; previous providers are restored where
StepThumbs still owns the hooks. Settings and cached images are kept.

Requires Windows 10/11 (64-bit), with a modern CPU and compatible GPU.

## Build

Clone with `--recurse-submodules`. With Rust/MSVC and the Windows SDK installed:

```sh
cargo build --release --locked
```

`cargo xtask dist` builds the installer with Inno Setup 7.1.1 installed.

## Credits

MIT licensed. Rendering is based on the excellent
[Look](https://github.com/stefangolas/look) by Stefan Golas and contributors.
[Our fork](https://github.com/stg/look/tree/stepthumbs-renderer) retains its
MIT OR Apache-2.0 license. Look's STEP coverage is incomplete.

Thanks also to [F3D](https://f3d.app/), the fast and minimalist 3D viewer by the
F3D-APP Foundation. It remains selectable if installed separately.
