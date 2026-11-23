# OMCLauncher（免安装版）

解压后直接运行 `./mclauncher`，不写入系统目录。

- 数据目录：**就在本文件同级**的 `mclauncher-data/`（解压包自带 `mclauncher.portable`
  标记文件，所以默认便携）。删掉该标记文件即改为系统默认位置
  `$XDG_DATA_HOME/mclauncher`（缺省 `~/.local/share/mclauncher`）；
  也可以用 `MCL_DATA_DIR=/path` 显式指定。
- Java：无需预装。启动器会按游戏版本自动探测/下载运行时，存放在数据目录下。
- 桌面图标：把 `mclauncher.desktop` 放到 `~/.local/share/applications/`，
  `mclauncher.png` 放到 `~/.local/share/icons/hicolor/256x256/apps/`。
  注意：桌面入口启动时工作目录不同，便携判定只看**可执行文件位置**，不受影响。

卸载：删除二进制与数据目录即可。

许可证：GPL-3.0-only。内置图标素材来自 HMCL（GPL-3.0-or-later），
详见仓库内 `crates/mcl-app-model/assets/icons/NOTICE.md`。
