using System;
using System.Drawing;
using System.IO;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using System.Threading.Tasks;
using System.Windows.Forms;

namespace WeaselUserDictSync
{
    internal sealed class MainForm : Form
    {
        private readonly ProgressBar progress = new ProgressBar();
        private readonly TextBox log = new TextBox();
        private readonly Button rime = new Button(), webdav = new Button();
        private readonly Button start = new Button(), stop = new Button();
        private readonly string baseDir = AppDomain.CurrentDomain.BaseDirectory;
        private readonly string iniPath;
        private CancellationTokenSource cancellation;

        public MainForm()
        {
            iniPath = Path.Combine(baseDir, "WeaselUserDictSync.ini");
            Text = "RIME 用户词库同步工具";
            Icon = Icon.ExtractAssociatedIcon(Application.ExecutablePath);
            ClientSize = new Size(760, 520);
            MinimumSize = new Size(620, 420);
            StartPosition = FormStartPosition.CenterScreen;

            progress.SetBounds(16, 16, 728, 24);
            progress.Anchor = AnchorStyles.Top | AnchorStyles.Left | AnchorStyles.Right;
            log.SetBounds(16, 52, 728, 350);
            log.Anchor = AnchorStyles.Top | AnchorStyles.Bottom | AnchorStyles.Left | AnchorStyles.Right;
            log.Multiline = true; log.ReadOnly = true; log.ScrollBars = ScrollBars.Both;
            log.Font = new Font("Consolas", 9F); log.WordWrap = false;

            rime.Text = "指定RIME用户词库"; webdav.Text = "设置WebDAV";
            rime.SetBounds(16, 414, 180, 32); webdav.SetBounds(212, 414, 150, 32);
            start.Text = "开始同步"; stop.Text = "停止同步";
            start.SetBounds(514, 414, 110, 32); stop.SetBounds(634, 414, 110, 32);
            foreach (Control c in new Control[] { rime, webdav, start, stop }) c.Anchor = AnchorStyles.Bottom | AnchorStyles.Left;
            start.Anchor = stop.Anchor = AnchorStyles.Bottom | AnchorStyles.Right;
            stop.Enabled = false;
            Controls.AddRange(new Control[] { progress, log, rime, webdav, start, stop });

            rime.Click += (s, e) => ConfigureRime();
            webdav.Click += (s, e) => ConfigureWebDav();
            start.Click += async (s, e) => await StartSync();
            stop.Click += (s, e) => { if (cancellation != null) cancellation.Cancel(); };
            Program.LogWritten += OnLog;
            FormClosed += (s, e) => { Program.LogWritten -= OnLog; if (cancellation != null) cancellation.Cancel(); };
            EnsureIni();
            if (File.Exists(Program.LogPath)) log.Text = File.ReadAllText(Program.LogPath, Encoding.UTF8);
        }

        private void EnsureIni()
        {
            if (!File.Exists(iniPath))
            {
                string example = Path.Combine(baseDir, "UserDictSync.ini.example");
                if (File.Exists(example)) File.Copy(example, iniPath);
                else
                {
                    string initial =
                        "[weasel]" + Environment.NewLine +
                        "user_data_dir=" + Environment.NewLine +
                        "deployer_path=" + Environment.NewLine + Environment.NewLine +
                        "[webdav]" + Environment.NewLine +
                        "url=" + Environment.NewLine +
                        "username=" + Environment.NewLine +
                        "password_base64=" + Environment.NewLine;
                    File.WriteAllText(iniPath, initial, new UTF8Encoding(false));
                }
            }
            Directory.CreateDirectory(Path.Combine(baseDir, "Sync", "WebDAV"));
        }

        private async Task StartSync()
        {
            if (!File.Exists(iniPath)) { MessageBox.Show(this, "找不到 WeaselUserDictSync.ini。", Text, MessageBoxButtons.OK, MessageBoxIcon.Error); return; }
            SetBusy(true); progress.Value = 0; cancellation = new CancellationTokenSource();
            try
            {
                var reporter = new Progress<int>(v => progress.Value = Math.Max(0, Math.Min(100, v)));
                await Program.Run(baseDir, cancellation.Token, reporter);
                MessageBox.Show(this, "同步完成。", Text, MessageBoxButtons.OK, MessageBoxIcon.Information);
            }
            catch (OperationCanceledException) { Program.Log("用户已停止同步。"); }
            catch (Exception ex)
            {
                Program.Log("失败: " + ex.Message);
                MessageBox.Show(this, ex.Message, Text, MessageBoxButtons.OK, MessageBoxIcon.Error);
            }
            finally { cancellation.Dispose(); cancellation = null; SetBusy(false); }
        }

        private void SetBusy(bool busy)
        {
            rime.Enabled = webdav.Enabled = start.Enabled = !busy;
            stop.Enabled = busy;
        }

        private void OnLog(string line)
        {
            if (IsDisposed) return;
            if (InvokeRequired) { BeginInvoke(new Action<string>(OnLog), line); return; }
            log.AppendText(line + Environment.NewLine); log.SelectionStart = log.TextLength; log.ScrollToCaret();
        }

        private void ConfigureWebDav()
        {
            Ini ini = Ini.Load(iniPath);
            string encoded = ini.Get("webdav", "password_base64", "");
            string password = "";
            try { if (encoded.Length > 0) password = Encoding.UTF8.GetString(Convert.FromBase64String(encoded)); } catch { }
            using (var dialog = new WebDavSettings(ini.Get("webdav", "url", ""), ini.Get("webdav", "username", ""), password))
            {
                if (dialog.ShowDialog(this) != DialogResult.OK) return;
                ini.Set("webdav", "url", dialog.Url);
                ini.Set("webdav", "username", dialog.Username);
                ini.Set("webdav", "password_base64", Convert.ToBase64String(Encoding.UTF8.GetBytes(dialog.Password)));
                ini.Remove("webdav", "password"); ini.Save(iniPath);
                Program.Log("已保存 WebDAV 设置。");
            }
        }

        private void ConfigureRime()
        {
            Ini ini = Ini.Load(iniPath);
            string current = Environment.ExpandEnvironmentVariables(ini.Get("weasel", "user_data_dir", ""));
            using (var picker = new FolderBrowserDialog { Description = "选择包含 installation.yaml 的 RIME 用户词库文件夹", SelectedPath = current })
            {
                if (picker.ShowDialog(this) != DialogResult.OK) return;
                string installation = Path.Combine(picker.SelectedPath, "installation.yaml");
                if (!File.Exists(installation))
                {
                    MessageBox.Show(this, "所选文件夹中找不到 installation.yaml。", Text, MessageBoxButtons.OK, MessageBoxIcon.Error);
                    return;
                }
                string fixedRoot = Path.GetFullPath(Path.Combine(baseDir, "Sync"));
                string yaml = File.ReadAllText(installation, Encoding.UTF8);
                string installationId = GetYamlScalar(yaml, "installation_id");
                Program.ValidateInstallationId(installationId);
                string fixedSyncFolder = Path.Combine(fixedRoot, installationId);
                yaml = SetYamlScalar(yaml, "sync_dir", fixedRoot);
                File.WriteAllText(installation, yaml, new UTF8Encoding(false));
                ini.Set("weasel", "user_data_dir", picker.SelectedPath); ini.Save(iniPath);
                Directory.CreateDirectory(Path.Combine(fixedRoot, "WebDAV"));
                Directory.CreateDirectory(fixedSyncFolder);
                Program.Log("已指定 RIME 用户词库: " + picker.SelectedPath);
                Program.Log("已将用户词库 installation.yaml 的 sync_dir 设置为: " + fixedRoot);
                Program.Log("实际同步文件夹为: " + fixedSyncFolder);
                Program.Log("用户词库 installation.yaml 的 installation_id 保持不变: " + installationId);
            }
        }

        private static string SetYamlScalar(string yaml, string key, string value)
        {
            string quoted = "'" + value.Replace("'", "''") + "'";
            var rx = new Regex(@"(?m)^(\s*" + Regex.Escape(key) + @"\s*:\s*).*$");
            return rx.IsMatch(yaml) ? rx.Replace(yaml, m => m.Groups[1].Value + quoted, 1) :
                yaml.TrimEnd() + Environment.NewLine + key + ": " + quoted + Environment.NewLine;
        }

        private static string GetYamlScalar(string yaml, string key)
        {
            var rx = new Regex(@"(?m)^\s*" + Regex.Escape(key) + @"\s*:\s*(.*?)\s*$");
            Match match = rx.Match(yaml);
            if (!match.Success) throw new InvalidDataException("installation.yaml 缺少 " + key + "。");
            string value = match.Groups[1].Value.Trim();
            if (value.Length >= 2 && ((value[0] == '\'' && value[value.Length - 1] == '\'') || (value[0] == '"' && value[value.Length - 1] == '"')))
                value = value.Substring(1, value.Length - 2);
            return value;
        }
    }

    internal sealed class WebDavSettings : Form
    {
        private readonly TextBox url = new TextBox(), username = new TextBox(), password = new TextBox();
        public string Url { get { return url.Text.Trim(); } }
        public string Username { get { return username.Text; } }
        public string Password { get { return password.Text; } }
        public WebDavSettings(string u, string n, string p)
        {
            Text = "WebDAV设置"; ClientSize = new Size(500, 198); StartPosition = FormStartPosition.CenterParent;
            FormBorderStyle = FormBorderStyle.FixedDialog; MaximizeBox = MinimizeBox = false;
            var labels = new[] { new Label { Text = "地址", AutoSize = true }, new Label { Text = "用户名", AutoSize = true }, new Label { Text = "密码", AutoSize = true } };
            for (int i = 0; i < 3; i++) labels[i].SetBounds(18, 23 + i * 43, 65, 24);
            url.SetBounds(90, 18, 390, 27); username.SetBounds(90, 61, 390, 27); password.SetBounds(90, 104, 390, 27);
            url.Text = u; username.Text = n; password.Text = p; password.UseSystemPasswordChar = true;
            var test = new Button { Text = "测试连接" };
            var ok = new Button { Text = "保存", DialogResult = DialogResult.OK }; var cancel = new Button { Text = "取消", DialogResult = DialogResult.Cancel };
            test.SetBounds(190, 151, 100, 30); ok.SetBounds(300, 151, 85, 30); cancel.SetBounds(395, 151, 85, 30);
            test.Click += async (s, e) =>
            {
                if (Url.Length == 0 || Username.Length == 0)
                {
                    MessageBox.Show(this, "请先填写 WebDAV 地址和用户名。", Text, MessageBoxButtons.OK, MessageBoxIcon.Warning);
                    return;
                }
                test.Enabled = ok.Enabled = cancel.Enabled = false;
                test.Text = "测试中...";
                try
                {
                    Uri endpoint;
                    if (!Uri.TryCreate(Url.TrimEnd('/') + "/", UriKind.Absolute, out endpoint))
                        throw new InvalidDataException("WebDAV 地址格式不正确。");
                    using (var client = new WebDavClient(endpoint, Username, Password))
                        await client.TestUploadDownload(CancellationToken.None);
                    Program.Log("WebDAV 测试连接成功，临时文件上传、下载和校验均通过。");
                    MessageBox.Show(this, "连接成功，上传和下载测试均已通过。", Text, MessageBoxButtons.OK, MessageBoxIcon.Information);
                }
                catch (Exception ex)
                {
                    Program.Log("WebDAV 测试连接失败: " + ex.Message);
                    MessageBox.Show(this, "测试失败：" + ex.Message, Text, MessageBoxButtons.OK, MessageBoxIcon.Error);
                }
                finally { test.Text = "测试连接"; test.Enabled = ok.Enabled = cancel.Enabled = true; }
            };
            Controls.AddRange(new Control[] { labels[0], labels[1], labels[2], url, username, password, test, ok, cancel });
            AcceptButton = ok; CancelButton = cancel;
        }
    }
}
