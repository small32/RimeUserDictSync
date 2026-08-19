using Microsoft.Win32;
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Globalization;
using System.Linq;
using System.Net;
using System.Net.Http;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading.Tasks;
using System.Threading;
using System.Windows.Forms;
using System.Xml.Linq;

namespace WeaselUserDictSync
{
    internal static class Program
    {
        internal static string LogPath;
        internal static event Action<string> LogWritten;

        [STAThread]
        private static int Main(string[] args)
        {
            string baseDir = AppDomain.CurrentDomain.BaseDirectory;
            LogPath = Path.Combine(baseDir, "RimeSync.log");
            try
            {
                if (args.Length == 1 && args[0] == "--self-test")
                    return SelfTest();
                Application.EnableVisualStyles();
                Application.SetCompatibleTextRenderingDefault(false);
                Application.Run(new MainForm());
                return 0;
            }
            catch (Exception ex)
            {
                Log("失败: " + ex.Message);
                return 1;
            }
        }

        internal static async Task Run(string baseDir, CancellationToken cancellation, IProgress<int> progress)
        {
            Action<int> report = value => { if (progress != null) progress.Report(value); };
            report(0);
            string iniPath = Path.Combine(baseDir, "WeaselUserDictSync.ini");
            if (!File.Exists(iniPath))
                throw new FileNotFoundException("找不到配置文件，请复制并填写 WeaselUserDictSync.ini。", iniPath);
            Ini ini = Ini.Load(iniPath);
            string userDir = Expand(ini.Get("weasel", "user_data_dir", Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "Rime")));
            string installYaml = Path.Combine(userDir, "installation.yaml");
            if (!File.Exists(installYaml))
                throw new FileNotFoundException("找不到 RIME installation.yaml，请先点击“指定RIME用户词库”。", installYaml);

            string syncRoot = Expand(ReadYamlScalar(installYaml, "sync_dir"));
            string installationId = ReadYamlScalar(installYaml, "installation_id");
            ValidateInstallationId(installationId);
            if (!Path.IsPathRooted(syncRoot)) syncRoot = Path.GetFullPath(Path.Combine(userDir, syncRoot));
            string fixedRoot = Path.GetFullPath(Path.Combine(baseDir, "Sync"));
            string syncFolder = Path.Combine(fixedRoot, installationId);
            if (!String.Equals(Path.GetFullPath(syncRoot).TrimEnd(Path.DirectorySeparatorChar), fixedRoot.TrimEnd(Path.DirectorySeparatorChar), StringComparison.OrdinalIgnoreCase))
                throw new InvalidDataException("RIME 同步目录配置不正确，请重新点击“指定RIME用户词库”。");
            string localFolder = Path.Combine(fixedRoot, "WebDAV");
            ValidateLayout(fixedRoot, syncFolder, localFolder);

            string deployer = FindDeployer(Expand(ini.Get("weasel", "deployer_path", "")), baseDir);
            Uri webDavUri = NormalizeBaseUri(ini.Required("webdav", "url"));
            string username = ini.Required("webdav", "username");
            string password64 = ini.Get("webdav", "password_base64", "");
            string password = password64.Length > 0 ? Encoding.UTF8.GetString(Convert.FromBase64String(password64)) : ini.Required("webdav", "password");

            Log("步骤 1/7：第 1 次运行 RIME 用户资料同步。");
            RunDeployer(deployer, "/sync", cancellation);
            report(15);
            Directory.CreateDirectory(syncFolder);
            CopyDirectoryIfPresent(Path.Combine(userDir, "cn_dicts"), Path.Combine(syncFolder, "cn_dicts"));
            CopyDirectoryIfPresent(Path.Combine(userDir, "en_dicts"), Path.Combine(syncFolder, "en_dicts"));
            CopyFileIfPresent(Path.Combine(userDir, "wanxiang-lts-zh-hans.gram"), Path.Combine(syncFolder, "wanxiang-lts-zh-hans.gram"));
            Log("步骤 1/7：已将 RIME 用户词库目录中的整个 cn_dicts、en_dicts 及 wanxiang-lts-zh-hans.gram 复制到 Sync\\" + installationId + "。");
            report(25);

            using (WebDavClient dav = new WebDavClient(webDavUri, username, password))
            {
                cancellation.ThrowIfCancellationRequested();
                List<WebDavFile> remoteFiles = await dav.ListFilesRecursive(cancellation);
                if (remoteFiles.Count == 0)
                {
                    Log("步骤 2/7：WebDAV 远端为空，使用当前同步数据初始化并直接上传。");
                    RecreateDirectory(localFolder);
                    CopyDirectory(syncFolder, localFolder, true);
                    bool corrected = EnsureWebDavInstallation(localFolder);
                    Log(corrected ? "步骤 2/7：已将 Sync\\WebDAV\\installation.yaml 的 installation_id 修正为 WebDAV。" :
                        "步骤 2/7：Sync\\WebDAV\\installation.yaml 的 installation_id 已是 WebDAV。 ");
                    await dav.UploadDirectory(localFolder, cancellation);
                }
                else
                {
                    Log("步骤 2/7：下载 WebDAV 文件: " + remoteFiles.Count + " 个。");
                    RecreateDirectory(localFolder);
                    await dav.DownloadFiles(remoteFiles, localFolder, cancellation);
                    bool corrected = EnsureWebDavInstallation(localFolder);
                    Log(corrected ? "步骤 2/7：下载后检查发现 installation_id 不符，已修正为 WebDAV。" :
                        "步骤 2/7：下载后检查完成，installation_id 已是 WebDAV。");
                }
                Log("步骤 3/7：已确认本地文件夹与同步文件夹位于同一同步根目录下。");
                report(45);

                Log("步骤 4/7：第 2 次运行 RIME 用户资料同步。");
                RunDeployer(deployer, "/sync", cancellation);
                report(65);

                MergeDictDirectories(localFolder, syncFolder, "cn_dicts");
                MergeDictDirectories(localFolder, syncFolder, "en_dicts");
                SynchronizeNewestFile(localFolder, syncFolder, "wanxiang-lts-zh-hans.gram");
                if (EnsureWebDavInstallation(localFolder))
                    Log("步骤 5/7：再次将 Sync\\WebDAV\\installation.yaml 的 installation_id 修正为 WebDAV。");
                Log("步骤 5/7：cn_dicts、en_dicts 正文并集及 gram 文件新旧比较完成，并已写回两侧。");
                Log("步骤 5/7：合并完成，开始运行 RIME 重新部署。");
                RunDeployer(deployer, "/deploy", cancellation);
                Log("步骤 5/7：RIME 重新部署完成。");
                report(80);

                Log("步骤 6/7：覆盖上传本地文件夹全部内容到 WebDAV。");
                await dav.UploadDirectory(localFolder, cancellation);
                report(95);
                Log("步骤 7/7：WebDAV 上传完成，开始清空本地文件夹和同步文件夹（保留目录）。");
                ClearDirectory(localFolder);
                ClearDirectory(syncFolder);
                Log("步骤 7/7：已清空本地文件夹和同步文件夹，目录已保留。");
            }
            report(100);
            Log("全部同步步骤已完成。");
        }

        private static void ValidateLayout(string syncRoot, string syncFolder, string localFolder)
        {
            string root = Path.GetFullPath(syncRoot).TrimEnd(Path.DirectorySeparatorChar);
            string localParent = Directory.GetParent(localFolder.TrimEnd(Path.DirectorySeparatorChar)).FullName;
            string syncParent = Directory.GetParent(syncFolder.TrimEnd(Path.DirectorySeparatorChar)).FullName;
            if (!String.Equals(root, localParent, StringComparison.OrdinalIgnoreCase) ||
                !String.Equals(root, syncParent, StringComparison.OrdinalIgnoreCase))
                throw new InvalidOperationException("local.folder 与当前设备同步文件夹必须同为 sync_dir 的直接子目录。");
            if (!String.Equals(new DirectoryInfo(localFolder).Name, "WebDAV", StringComparison.Ordinal))
                throw new InvalidOperationException("本地文件夹名称必须为 WebDAV。");
            if (String.Equals(syncFolder, localFolder, StringComparison.OrdinalIgnoreCase))
                throw new InvalidOperationException("当前设备 installation_id 不能是 WebDAV；它会与本地镜像冲突。");
        }

        private static string FindDeployer(string configured, string baseDir)
        {
            var candidates = new List<string>();
            if (!String.IsNullOrWhiteSpace(configured)) candidates.Add(configured);
            candidates.Add(Path.Combine(baseDir, "WeaselDeployer.exe"));
            foreach (RegistryView view in new[] { RegistryView.Registry64, RegistryView.Registry32 })
            {
                try
                {
                    using (RegistryKey root = RegistryKey.OpenBaseKey(RegistryHive.LocalMachine, view))
                    using (RegistryKey key = root.OpenSubKey(@"SOFTWARE\Rime\Weasel"))
                    {
                        if (key != null)
                        {
                            string dir = Convert.ToString(key.GetValue("WeaselRoot"));
                            if (!String.IsNullOrWhiteSpace(dir)) candidates.Add(Path.Combine(dir, "WeaselDeployer.exe"));
                        }
                    }
                }
                catch { }
            }
            string found = candidates.Select(Expand).FirstOrDefault(File.Exists);
            if (found == null) throw new FileNotFoundException("找不到 WeaselDeployer.exe，请在 INI 设置 deployer_path。");
            return found;
        }

        internal static void ValidateInstallationId(string installationId)
        {
            if (String.IsNullOrWhiteSpace(installationId) || installationId == "." || installationId == ".." ||
                installationId.IndexOfAny(Path.GetInvalidFileNameChars()) >= 0 ||
                installationId.IndexOf(Path.DirectorySeparatorChar) >= 0 || installationId.IndexOf(Path.AltDirectorySeparatorChar) >= 0 ||
                String.Equals(installationId, "WebDAV", StringComparison.OrdinalIgnoreCase))
                throw new InvalidDataException("installation_id 不能安全地用作同步文件夹名称: " + installationId);
        }

        private static void RunDeployer(string path, string arguments, CancellationToken cancellation)
        {
            var p = Process.Start(new ProcessStartInfo(path, arguments)
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                WorkingDirectory = Path.GetDirectoryName(path)
            });
            if (p == null) throw new InvalidOperationException("无法启动 WeaselDeployer.exe。");
            while (!p.WaitForExit(200))
            {
                if (cancellation.IsCancellationRequested)
                {
                    try { p.Kill(); } catch { }
                    cancellation.ThrowIfCancellationRequested();
                }
            }
            if (p.ExitCode != 0) throw new InvalidOperationException("RIME 用户资料同步失败，退出码 " + p.ExitCode + "。");
        }

        private static void MergeDictDirectories(string localRoot, string syncRoot, string name)
        {
            string local = Path.Combine(localRoot, name), sync = Path.Combine(syncRoot, name);
            Directory.CreateDirectory(local);
            Directory.CreateDirectory(sync);
            var relativeFiles = Directory.EnumerateFiles(local, "*", SearchOption.AllDirectories)
                .Select(x => Relative(local, x)).Concat(Directory.EnumerateFiles(sync, "*", SearchOption.AllDirectories)
                .Select(x => Relative(sync, x))).Distinct(StringComparer.OrdinalIgnoreCase).OrderBy(x => x).ToList();
            foreach (string rel in relativeFiles)
            {
                string a = Path.Combine(local, rel), b = Path.Combine(sync, rel);
                Directory.CreateDirectory(Path.GetDirectoryName(a));
                Directory.CreateDirectory(Path.GetDirectoryName(b));
                if (!File.Exists(a)) File.Copy(b, a, true);
                else if (!File.Exists(b)) File.Copy(a, b, true);
                else
                {
                    List<string> merged = OrderedDictionaryUnion(a, b);
                    WriteUtf8Lines(a, merged);
                    WriteUtf8Lines(b, merged);
                }
            }
            Log(name + " 合并文件: " + relativeFiles.Count + " 个。");
        }

        internal static List<string> OrderedLineUnion(string first, string second)
        {
            EnsureTextFile(first); EnsureTextFile(second);
            var result = new List<string>();
            var seen = new HashSet<string>(StringComparer.Ordinal);
            foreach (string line in File.ReadAllLines(first, Encoding.UTF8).Concat(File.ReadAllLines(second, Encoding.UTF8)))
                if (seen.Add(line)) result.Add(line);
            return result;
        }

        internal static List<string> OrderedDictionaryUnion(string first, string second)
        {
            EnsureTextFile(first); EnsureTextFile(second);
            string[] a = File.ReadAllLines(first, Encoding.UTF8), b = File.ReadAllLines(second, Encoding.UTF8);
            int aBody = HeaderEnd(a), bBody = HeaderEnd(b);
            var result = new List<string>();
            result.AddRange(a.Take(aBody));
            MergeDictionaryBody(result, a.Skip(aBody).Concat(b.Skip(bBody)));
            return result;
        }

        private static void MergeDictionaryBody(List<string> result, IEnumerable<string> lines)
        {
            var exactLines = new HashSet<string>(StringComparer.Ordinal);
            var weightedEntries = new Dictionary<string, Tuple<decimal, int>>(StringComparer.Ordinal);
            foreach (string line in lines)
            {
                string key; decimal weight;
                if (TryParseWeightedEntry(line, out key, out weight))
                {
                    Tuple<decimal, int> existing;
                    if (!weightedEntries.TryGetValue(key, out existing))
                    {
                        weightedEntries[key] = Tuple.Create(weight, result.Count);
                        result.Add(line);
                    }
                    else if (weight > existing.Item1)
                    {
                        result[existing.Item2] = line;
                        weightedEntries[key] = Tuple.Create(weight, existing.Item2);
                    }
                }
                else if (exactLines.Add(line)) result.Add(line);
            }
        }

        private static bool TryParseWeightedEntry(string line, out string key, out decimal weight)
        {
            key = null; weight = 0;
            int lastTab = line.LastIndexOf('\t');
            if (lastTab <= 0 || lastTab == line.Length - 1) return false;
            string number = line.Substring(lastTab + 1).Trim();
            if (!Decimal.TryParse(number, NumberStyles.Number, CultureInfo.InvariantCulture, out weight)) return false;
            key = line.Substring(0, lastTab);
            return true;
        }

        private static int HeaderEnd(string[] lines)
        {
            if (lines.Length == 0 || lines[0].Trim() != "---") return 0;
            for (int i = 1; i < lines.Length; i++)
                if (lines[i].Trim() == "...") return i + 1;
            throw new InvalidDataException("词库文件头以 --- 开始但缺少 ... 结束标记。");
        }

        private static void EnsureTextFile(string path)
        {
            byte[] bytes = File.ReadAllBytes(path);
            if (bytes.Any(b => b == 0)) throw new InvalidDataException("词库目录含二进制文件，无法逐行合并: " + path);
            try { new UTF8Encoding(false, true).GetString(RemoveUtf8Bom(bytes)); }
            catch (DecoderFallbackException) { throw new InvalidDataException("词库文件不是 UTF-8: " + path); }
        }

        private static byte[] RemoveUtf8Bom(byte[] b)
        {
            return b.Length >= 3 && b[0] == 0xEF && b[1] == 0xBB && b[2] == 0xBF ? b.Skip(3).ToArray() : b;
        }

        private static void WriteUtf8Lines(string path, List<string> lines)
        {
            File.WriteAllText(path, String.Join(Environment.NewLine, lines) + (lines.Count > 0 ? Environment.NewLine : ""), new UTF8Encoding(false));
        }

        private static bool EnsureWebDavInstallation(string folder)
        {
            Directory.CreateDirectory(folder);
            string path = Path.Combine(folder, "installation.yaml");
            string text = File.Exists(path) ? File.ReadAllText(path, Encoding.UTF8) : "";
            var rx = new Regex(@"(?m)^(\s*installation_id\s*:\s*).*$");
            string updated = rx.IsMatch(text) ? rx.Replace(text, "$1WebDAV", 1) : "installation_id: WebDAV\r\n" + text;
            bool changed = !String.Equals(text, updated, StringComparison.Ordinal);
            if (changed) File.WriteAllText(path, updated, new UTF8Encoding(false));
            return changed;
        }

        private static string ReadYamlScalar(string path, string key)
        {
            var rx = new Regex(@"^\s*" + Regex.Escape(key) + @"\s*:\s*(.*?)\s*$");
            foreach (string line in File.ReadLines(path, Encoding.UTF8))
            {
                Match m = rx.Match(line);
                if (m.Success)
                {
                    string value = m.Groups[1].Value;
                    int comment = value.IndexOf(" #", StringComparison.Ordinal);
                    if (comment >= 0) value = value.Substring(0, comment).Trim();
                    if (value.Length >= 2 && ((value[0] == '\'' && value[value.Length - 1] == '\'') || (value[0] == '"' && value[value.Length - 1] == '"')))
                        value = value.Substring(1, value.Length - 2);
                    if (value.Length > 0) return value;
                }
            }
            throw new InvalidDataException("installation.yaml 缺少 " + key + "。");
        }

        private static void CopyDirectoryIfPresent(string source, string target)
        {
            if (Directory.Exists(source)) CopyDirectory(source, target, true);
        }

        private static void CopyFileIfPresent(string source, string target)
        {
            if (!File.Exists(source))
            {
                Log("用户词库目录中未找到 " + Path.GetFileName(source) + "，跳过复制。");
                return;
            }
            Directory.CreateDirectory(Path.GetDirectoryName(target));
            File.Copy(source, target, true);
            File.SetLastWriteTimeUtc(target, File.GetLastWriteTimeUtc(source));
        }

        private static void SynchronizeNewestFile(string localRoot, string syncRoot, string name)
        {
            string local = Path.Combine(localRoot, name), sync = Path.Combine(syncRoot, name);
            if (!File.Exists(local) && !File.Exists(sync))
            {
                Log(name + " 在 WebDAV 本地镜像和同步文件夹中均不存在，跳过。");
                return;
            }
            if (!File.Exists(local))
            {
                CopyFileIfPresent(sync, local);
                Log("WebDAV 中没有 " + name + "，已从同步文件夹复制过去。");
                return;
            }
            if (!File.Exists(sync))
            {
                CopyFileIfPresent(local, sync);
                Log("同步文件夹中没有 " + name + "，已从 WebDAV 镜像复制过去。");
                return;
            }
            DateTime localTime = File.GetLastWriteTimeUtc(local), syncTime = File.GetLastWriteTimeUtc(sync);
            if (localTime > syncTime)
            {
                CopyFileIfPresent(local, sync);
                Log("WebDAV 中的 " + name + " 修改日期较新，已覆盖同步文件夹中的旧文件。");
            }
            else if (syncTime > localTime)
            {
                CopyFileIfPresent(sync, local);
                Log("同步文件夹中的 " + name + " 修改日期较新，已覆盖 WebDAV 镜像中的旧文件。");
            }
            else Log(name + " 两侧修改日期相同，无需覆盖。");
        }

        private static void CopyDirectory(string source, string target, bool overwrite)
        {
            Directory.CreateDirectory(target);
            foreach (string dir in Directory.EnumerateDirectories(source, "*", SearchOption.AllDirectories))
                Directory.CreateDirectory(Path.Combine(target, Relative(source, dir)));
            foreach (string file in Directory.EnumerateFiles(source, "*", SearchOption.AllDirectories))
            {
                string dest = Path.Combine(target, Relative(source, file));
                Directory.CreateDirectory(Path.GetDirectoryName(dest));
                File.Copy(file, dest, overwrite);
            }
        }

        private static void RecreateDirectory(string path)
        {
            if (Directory.Exists(path)) Directory.Delete(path, true);
            Directory.CreateDirectory(path);
        }

        private static void ClearDirectory(string path)
        {
            Directory.CreateDirectory(path);
            foreach (string file in Directory.EnumerateFiles(path, "*", SearchOption.TopDirectoryOnly)) File.Delete(file);
            foreach (string dir in Directory.EnumerateDirectories(path, "*", SearchOption.TopDirectoryOnly)) Directory.Delete(dir, true);
        }

        private static string Relative(string root, string path)
        {
            string prefix = Path.GetFullPath(root).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            string full = Path.GetFullPath(path);
            if (!full.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) throw new InvalidOperationException("路径越界: " + path);
            return full.Substring(prefix.Length);
        }

        private static string Expand(string value) { return Environment.ExpandEnvironmentVariables(value ?? "").Trim(); }
        private static Uri NormalizeBaseUri(string value) { return new Uri(value.TrimEnd('/') + "/", UriKind.Absolute); }
        internal static void Log(string message)
        {
            string line = DateTime.Now.ToString("yyyyMMddHHmmss") + " " + message;
            File.AppendAllText(LogPath, line + Environment.NewLine, Encoding.UTF8);
            Action<string> handler = LogWritten;
            if (handler != null) handler(line);
        }

        private static int SelfTest()
        {
            string root = Path.Combine(Path.GetTempPath(), "WeaselUserDictSyncTest-" + Guid.NewGuid().ToString("N"));
            try
            {
                Directory.CreateDirectory(root);
                string a = Path.Combine(root, "a.txt"), b = Path.Combine(root, "b.txt");
                File.WriteAllText(a, "A\nB\nC\n", new UTF8Encoding(false));
                File.WriteAllText(b, "B\nC\nD\n", new UTF8Encoding(false));
                string actual = String.Join(",", OrderedLineUnion(a, b));
                if (actual != "A,B,C,D") throw new Exception("并集测试失败: " + actual);
                File.WriteAllText(a, "---\nname: test\nversion: \"1\"\nsort: by_weight\n...\nA\nB\nC\n𤭢\tcei\t800\n", new UTF8Encoding(false));
                File.WriteAllText(b, "---\nname: test\nversion: \"2\"\nsort: by_weight\n...\nB\nC\nD\n𤭢\tcei\t1000\n", new UTF8Encoding(false));
                string dict = String.Join("|", OrderedDictionaryUnion(a, b));
                if (dict != "---|name: test|version: \"1\"|sort: by_weight|...|A|B|C|𤭢\tcei\t1000|D") throw new Exception("跳过文件头及权重并集测试失败: " + dict);
                EnsureWebDavInstallation(root);
                if (ReadYamlScalar(Path.Combine(root, "installation.yaml"), "installation_id") != "WebDAV")
                    throw new Exception("installation_id 测试失败。");
                string localRoot = Path.Combine(root, "local"), syncRoot = Path.Combine(root, "sync");
                Directory.CreateDirectory(localRoot); Directory.CreateDirectory(syncRoot);
                string localGram = Path.Combine(localRoot, "wanxiang-lts-zh-hans.gram");
                string syncGram = Path.Combine(syncRoot, "wanxiang-lts-zh-hans.gram");
                File.WriteAllText(localGram, "old", Encoding.UTF8);
                File.SetLastWriteTimeUtc(localGram, new DateTime(2025, 1, 1, 0, 0, 0, DateTimeKind.Utc));
                File.WriteAllText(syncGram, "new", Encoding.UTF8);
                File.SetLastWriteTimeUtc(syncGram, new DateTime(2026, 1, 1, 0, 0, 0, DateTimeKind.Utc));
                SynchronizeNewestFile(localRoot, syncRoot, "wanxiang-lts-zh-hans.gram");
                if (File.ReadAllText(localGram, Encoding.UTF8) != "new") throw new Exception("gram 修改日期覆盖测试失败。");
                Console.WriteLine("Self-test passed.");
                return 0;
            }
            finally { if (Directory.Exists(root)) Directory.Delete(root, true); }
        }
    }

    internal sealed class Ini
    {
        private readonly Dictionary<string, string> values = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        public static Ini Load(string path)
        {
            var ini = new Ini(); string section = "";
            foreach (string raw in File.ReadAllLines(path, Encoding.UTF8))
            {
                string line = raw.Trim();
                if (line.Length == 0 || line.StartsWith(";") || line.StartsWith("#")) continue;
                if (line.StartsWith("[") && line.EndsWith("]")) { section = line.Substring(1, line.Length - 2).Trim(); continue; }
                int eq = line.IndexOf('=');
                if (eq > 0) ini.values[section + "\0" + line.Substring(0, eq).Trim()] = line.Substring(eq + 1).Trim();
            }
            return ini;
        }
        public string Get(string section, string key, string fallback)
        {
            string value; return values.TryGetValue(section + "\0" + key, out value) && value.Length > 0 ? value : fallback;
        }
        public string Required(string section, string key)
        {
            string value = Get(section, key, "");
            if (String.IsNullOrWhiteSpace(value)) throw new InvalidDataException("INI 缺少 [" + section + "] " + key + "。");
            return value;
        }
        public void Set(string section, string key, string value) { values[section + "\0" + key] = value ?? ""; }
        public void Remove(string section, string key) { values.Remove(section + "\0" + key); }
        public void Save(string path)
        {
            var sections = values.Keys.Select(k => k.Split('\0')[0]).Distinct(StringComparer.OrdinalIgnoreCase);
            var output = new List<string>();
            foreach (string section in sections)
            {
                output.Add("[" + section + "]");
                foreach (var pair in values.Where(p => p.Key.StartsWith(section + "\0", StringComparison.OrdinalIgnoreCase)))
                    output.Add(pair.Key.Substring(pair.Key.IndexOf('\0') + 1) + "=" + pair.Value);
                output.Add("");
            }
            File.WriteAllLines(path, output, new UTF8Encoding(false));
        }
    }

    internal sealed class WebDavFile
    {
        public string RelativePath;
        public DateTime? LastModifiedUtc;
    }

    internal sealed class WebDavClient : IDisposable
    {
        private readonly Uri root;
        private readonly HttpClient http;
        public WebDavClient(Uri root, string user, string password)
        {
            this.root = root;
            var handler = new HttpClientHandler { Credentials = new NetworkCredential(user, password), PreAuthenticate = true };
            http = new HttpClient(handler) { Timeout = TimeSpan.FromMinutes(10) };
        }

        public async Task TestUploadDownload(CancellationToken cancellation)
        {
            string relative = ".rime-sync-test-" + Guid.NewGuid().ToString("N") + ".tmp";
            byte[] expected = Encoding.UTF8.GetBytes("RIME WebDAV test " + Guid.NewGuid().ToString("N"));
            bool uploaded = false;
            Exception failure = null;
            try
            {
                await EnsureCollection("", cancellation);
                using (var content = new ByteArrayContent(expected))
                {
                    HttpResponseMessage put = await http.PutAsync(UriFor(relative, false), content, cancellation);
                    EnsureSuccess(put, "测试上传");
                    uploaded = true;
                }
                HttpResponseMessage get = await http.GetAsync(UriFor(relative, false), cancellation);
                EnsureSuccess(get, "测试下载");
                byte[] actual = await get.Content.ReadAsByteArrayAsync();
                if (!expected.SequenceEqual(actual))
                    throw new InvalidDataException("测试文件下载内容与上传内容不一致。");
            }
            catch (Exception ex) { failure = ex; }
            if (uploaded)
            {
                try
                {
                    var delete = new HttpRequestMessage(HttpMethod.Delete, UriFor(relative, false));
                    HttpResponseMessage removed = await http.SendAsync(delete, cancellation);
                    EnsureSuccess(removed, "删除 WebDAV 临时测试文件");
                }
                catch (Exception ex)
                {
                    if (failure == null) failure = ex;
                    else failure = new InvalidOperationException(failure.Message + "；同时无法删除临时测试文件: " + ex.Message, failure);
                }
            }
            if (failure != null) throw new InvalidOperationException(failure.Message, failure);
        }

        public async Task<List<WebDavFile>> ListFilesRecursive(CancellationToken cancellation)
        {
            var files = new List<WebDavFile>();
            await Walk("", files, cancellation);
            return files;
        }

        private async Task Walk(string relative, List<WebDavFile> files, CancellationToken cancellation)
        {
            cancellation.ThrowIfCancellationRequested();
            Uri uri = UriFor(relative, true);
            var request = new HttpRequestMessage(new HttpMethod("PROPFIND"), uri);
            request.Headers.Add("Depth", "1");
            request.Content = new StringContent("<?xml version=\"1.0\"?><propfind xmlns=\"DAV:\"><prop><resourcetype/><getlastmodified/></prop></propfind>", Encoding.UTF8, "application/xml");
            HttpResponseMessage response = await http.SendAsync(request, cancellation);
            if (response.StatusCode == HttpStatusCode.NotFound) return;
            EnsureSuccess(response, "列出 WebDAV");
            XNamespace d = "DAV:";
            XDocument document;
            using (Stream xmlStream = await response.Content.ReadAsStreamAsync())
                document = XDocument.Load(xmlStream);
            foreach (XElement item in document.Descendants(d + "response"))
            {
                string href = (string)item.Element(d + "href");
                if (String.IsNullOrEmpty(href)) continue;
                Uri itemUri = new Uri(root, href);
                string rel = Uri.UnescapeDataString(root.MakeRelativeUri(itemUri).ToString()).Trim('/').Replace('/', Path.DirectorySeparatorChar);
                if (rel.Length == 0 || rel == relative.TrimEnd(Path.DirectorySeparatorChar)) continue;
                bool collection = item.Descendants(d + "collection").Any();
                if (collection) await Walk(rel, files, cancellation);
                else
                {
                    DateTimeOffset modified;
                    string rawModified = (string)item.Descendants(d + "getlastmodified").FirstOrDefault();
                    files.Add(new WebDavFile
                    {
                        RelativePath = rel,
                        LastModifiedUtc = DateTimeOffset.TryParse(rawModified, out modified) ? (DateTime?)modified.UtcDateTime : null
                    });
                }
            }
        }

        public async Task DownloadFiles(IEnumerable<WebDavFile> files, string localRoot, CancellationToken cancellation)
        {
            foreach (WebDavFile remote in files)
            {
                string rel = remote.RelativePath;
                cancellation.ThrowIfCancellationRequested();
                string local = SafeLocal(localRoot, rel);
                Directory.CreateDirectory(Path.GetDirectoryName(local));
                HttpResponseMessage response = await http.GetAsync(UriFor(rel, false), cancellation);
                EnsureSuccess(response, "下载 " + rel);
                using (Stream input = await response.Content.ReadAsStreamAsync())
                using (FileStream output = File.Create(local)) await input.CopyToAsync(output);
                if (remote.LastModifiedUtc.HasValue) File.SetLastWriteTimeUtc(local, remote.LastModifiedUtc.Value);
            }
        }

        public async Task UploadDirectory(string localRoot, CancellationToken cancellation)
        {
            await EnsureCollection("", cancellation);
            foreach (string dir in Directory.EnumerateDirectories(localRoot, "*", SearchOption.AllDirectories).OrderBy(x => x.Length))
                await EnsureCollection(RelativeLocal(localRoot, dir), cancellation);
            foreach (string file in Directory.EnumerateFiles(localRoot, "*", SearchOption.AllDirectories))
            {
                cancellation.ThrowIfCancellationRequested();
                string rel = RelativeLocal(localRoot, file);
                using (var content = new StreamContent(File.OpenRead(file)))
                {
                    HttpResponseMessage response = await http.PutAsync(UriFor(rel, false), content, cancellation);
                    EnsureSuccess(response, "上传 " + rel);
                }
            }
        }

        private async Task EnsureCollection(string relative, CancellationToken cancellation)
        {
            Uri uri = UriFor(relative, true);
            var probe = new HttpRequestMessage(new HttpMethod("PROPFIND"), uri); probe.Headers.Add("Depth", "0");
            HttpResponseMessage exists = await http.SendAsync(probe, cancellation);
            if (exists.IsSuccessStatusCode || (int)exists.StatusCode == 207) return;
            if (exists.StatusCode != HttpStatusCode.NotFound) EnsureSuccess(exists, "检查 WebDAV 目录");
            var mkcol = new HttpRequestMessage(new HttpMethod("MKCOL"), uri);
            HttpResponseMessage made = await http.SendAsync(mkcol, cancellation);
            EnsureSuccess(made, "创建 WebDAV 目录 " + relative);
        }

        private Uri UriFor(string relative, bool directory)
        {
            string escaped = String.Join("/", relative.Replace('\\', '/').Split(new[] { '/' }, StringSplitOptions.RemoveEmptyEntries).Select(Uri.EscapeDataString));
            if (directory && escaped.Length > 0) escaped += "/";
            return new Uri(root, escaped);
        }
        private static string SafeLocal(string root, string relative)
        {
            string basePath = Path.GetFullPath(root).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            string result = Path.GetFullPath(Path.Combine(root, relative));
            if (!result.StartsWith(basePath, StringComparison.OrdinalIgnoreCase)) throw new InvalidDataException("WebDAV 返回了越界路径。");
            return result;
        }
        private static string RelativeLocal(string root, string path)
        {
            string prefix = Path.GetFullPath(root).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            return Path.GetFullPath(path).Substring(prefix.Length);
        }
        private static void EnsureSuccess(HttpResponseMessage response, string action)
        {
            if (!response.IsSuccessStatusCode && (int)response.StatusCode != 207)
                throw new HttpRequestException(action + "失败: HTTP " + (int)response.StatusCode + " " + response.ReasonPhrase);
        }
        public void Dispose() { http.Dispose(); }
    }
}
