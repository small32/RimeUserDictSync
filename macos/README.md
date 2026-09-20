# macOS 图标

`AppIcon.png` 用于运行时 Dock 图标，`AppIcon.icns` 用于应用包和 Finder。
两者来自同一份矢量绘制代码 `generate-icon.swift`，保留 RIME 白色标识，
使用深灰渐变圆角底板、透明留白和轻微阴影。Windows 继续使用 `weasel.ico`。

在 macOS 仓库根目录重新生成（需要 Command Line Tools）：

```sh
ICON_WORK=$(mktemp -d)
swift macos/generate-icon.swift "$ICON_WORK/AppIcon.iconset"
iconutil -c icns "$ICON_WORK/AppIcon.iconset" -o macos/AppIcon.icns
cp "$ICON_WORK/AppIcon.iconset/icon_512x512@2x.png" macos/AppIcon.png
```

提交生成的 PNG 和 ICNS，构建时直接使用，不依赖额外图形库。
ICNS 包含 16–1024 像素的标准尺寸，兼容 macOS 11 及以上。
这是适配 Tahoe 外形的静态图标，不包含 Icon Composer 的动态 Liquid Glass 层或外观变体。
