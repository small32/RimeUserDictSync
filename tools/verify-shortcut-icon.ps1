# 验证「同步词库」快捷方式的图标。
#
# 用法（仓库根目录）：
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File tools\verify-shortcut-icon.ps1 -AppDir dist
#
# 判定依据（都在 .lnk 的真实字节上做，不只信 WScript 的读回值）：
#   1. ShellLinkHeader.IconIndex（偏移 56）= -101，负数表示"按资源 ID 取图标"
#   2. StringData 里的 ICON_LOCATION 只有裸路径，**不带 ",索引"**
#      —— 索引塞进路径字符串会让资源管理器把整个东西当文件名，
#         报「Windows 找不到文件 xxx.exe,-101」，这也是之前图标一直为空的原因
#   3. EXE 的索引 0 仍是 weasel，索引 -101 才是齿轮，两者必须不同
#
# 本脚本必须存为 UTF-8 **带 BOM**，否则 PS 5.1 会按 ANSI 解码脚本里的中文路径。

param(
    [string]$AppDir = "dist"
)

Add-Type -AssemblyName System.Drawing

$root = (Resolve-Path $AppDir).Path
$exe = Join-Path $root 'RimeUserDictSync.exe'
$lnk = Get-ChildItem $root -Filter '*.lnk' | Select-Object -First 1
$failed = @()

Write-Output "== 文件 =="
foreach ($item in @($exe, $lnk.FullName)) {
    $ok = Test-Path $item
    Write-Output ("  {0}  {1}" -f $(if ($ok) { "[有]" } else { "[缺]" }), $item)
    if (-not $ok) { $failed += "缺少 $item" }
}
$loose = Get-ChildItem $root -Filter '*.ico'
if ($loose) { $failed += ("目录里还有多余的图标文件: " + ($loose.Name -join ', ')) }
else { Write-Output "  [无]  多余的 .ico（图标取自 EXE 内嵌资源）" }

if (Test-Path $lnk.FullName) {
    $b = [System.IO.File]::ReadAllBytes($lnk.FullName)
    $iconIndex = [BitConverter]::ToInt32($b, 56)
    $flags = [BitConverter]::ToUInt32($b, 20)
    $pos = 0x4C
    if ($flags -band 1) { $size = [BitConverter]::ToUInt16($b, $pos); $pos += 2 + $size }
    if ($flags -band 2) { $size = [BitConverter]::ToUInt32($b, $pos); $pos += $size }
    $fields = @()
    if ($flags -band 0x4)  { $fields += 'NAME' }
    if ($flags -band 0x8)  { $fields += 'RELATIVE_PATH' }
    if ($flags -band 0x10) { $fields += 'WORKING_DIR' }
    if ($flags -band 0x20) { $fields += 'ARGUMENTS' }
    if ($flags -band 0x40) { $fields += 'ICON_LOCATION' }
    $unicode = [bool]($flags -band 0x80)
    $values = @{}
    Write-Output "== .lnk 实际字段 =="
    foreach ($name in $fields) {
        $count = [BitConverter]::ToUInt16($b, $pos); $pos += 2
        if ($unicode) { $s = [System.Text.Encoding]::Unicode.GetString($b, $pos, $count * 2); $pos += $count * 2 }
        else { $s = [System.Text.Encoding]::Default.GetString($b, $pos, $count); $pos += $count }
        $values[$name] = $s
        Write-Output ("  {0,-14} = '{1}'" -f $name, $s)
    }
    Write-Output ("  {0,-14} = {1}" -f 'Header.IconIndex', $iconIndex)

    if ($iconIndex -ne -101) { $failed += "Header.IconIndex 应为 -101，实际 $iconIndex" }
    if ($values['ICON_LOCATION'] -ne $exe) { $failed += "ICON_LOCATION 不是 EXE 的裸路径：$($values['ICON_LOCATION'])" }
    if ($values['ICON_LOCATION'] -like '*,*') { $failed += "ICON_LOCATION 里出现了逗号（索引不该写在这里）" }
    if ($values['ARGUMENTS'] -notlike '*--sync-now*') { $failed += "Arguments 里没有 --sync-now" }
    if ($values['WORKING_DIR'] -ne $root) { $failed += "WorkingDirectory 不是本目录" }
}

$code = @'
using System; using System.Drawing; using System.Runtime.InteropServices;
public class DefIcon {
  [DllImport("shell32.dll", CharSet=CharSet.Unicode)]
  static extern int SHDefExtractIconW(string file, int index, uint flags, out IntPtr large, out IntPtr small, uint size);
  [DllImport("user32.dll")] static extern bool DestroyIcon(IntPtr h);
  public static string Hash(string file, int index) {
    IntPtr large = IntPtr.Zero, small = IntPtr.Zero;
    if (SHDefExtractIconW(file, index, 0, out large, out small, 0x00200020) != 0 || large == IntPtr.Zero) return "FAIL";
    try {
      using (Icon icon = Icon.FromHandle(large))
      using (Bitmap bmp = icon.ToBitmap())
      using (var ms = new System.IO.MemoryStream()) {
        bmp.Save(ms, System.Drawing.Imaging.ImageFormat.Png);
        using (var sha = System.Security.Cryptography.SHA256.Create())
          return BitConverter.ToString(sha.ComputeHash(ms.ToArray())).Replace("-", "").Substring(0, 16);
      }
    } finally { DestroyIcon(large); if (small != IntPtr.Zero) DestroyIcon(small); }
  }
  public static bool Save(string file, int index, string outPng) {
    IntPtr large = IntPtr.Zero, small = IntPtr.Zero;
    if (SHDefExtractIconW(file, index, 0, out large, out small, 0x00200040) != 0 || large == IntPtr.Zero) return false;
    try {
      using (Icon icon = Icon.FromHandle(large))
      using (Bitmap bmp = icon.ToBitmap()) {
        var big = new Bitmap(64, 64);
        using (var g = Graphics.FromImage(big)) { g.Clear(Color.White); g.DrawImage(bmp, 0, 0, 64, 64); }
        big.Save(outPng, System.Drawing.Imaging.ImageFormat.Png);
        return true;
      }
    } finally { DestroyIcon(large); if (small != IntPtr.Zero) DestroyIcon(small); }
  }
}
'@
Add-Type -TypeDefinition $code -ReferencedAssemblies System.Drawing

$exeIcon = [DefIcon]::Hash($exe, 0)
$gearIcon = [DefIcon]::Hash($exe, -101)
Write-Output "== EXE 内嵌图标 =="
Write-Output ("  index 0    (EXE 自己的图标) : " + $exeIcon)
Write-Output ("  index -101 (快捷方式用的)   : " + $gearIcon)
if ($gearIcon -like "FAIL*") { $failed += "EXE 里取不到 ID 101 的图标资源" }
elseif ($gearIcon -eq $exeIcon) { $failed += "索引 -101 与索引 0 是同一个图标，说明齿轮没嵌进去或索引没生效" }

[void][DefIcon]::Save($exe, 0, (Join-Path $root 'check-exe-icon.png'))
[void][DefIcon]::Save($exe, -101, (Join-Path $root 'check-shortcut-icon.png'))
Write-Output "  已导出 check-exe-icon.png / check-shortcut-icon.png 供肉眼确认"

Write-Output ""
if ($failed.Count -gt 0) {
    Write-Output "结果: 失败"
    $failed | ForEach-Object { Write-Output ("  - " + $_) }
    exit 1
} else {
    Write-Output "结果: 通过 —— 只有 EXE 与 .lnk；ICON_LOCATION 是裸路径、IconIndex=-101；EXE 保持自己的图标，快捷方式用齿轮"
    exit 0
}
