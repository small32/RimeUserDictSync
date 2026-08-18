@echo off
setlocal
set "CSC=%WINDIR%\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
if not exist "%CSC%" set "CSC=%WINDIR%\Microsoft.NET\Framework\v4.0.30319\csc.exe"
if not exist "%CSC%" (
  echo Cannot find .NET Framework C# compiler.
  exit /b 1
)
"%CSC%" /nologo /optimize+ /target:winexe /win32icon:weasel.ico /out:WeaselUserDictSync.exe /reference:System.Net.Http.dll /reference:System.Xml.Linq.dll /reference:System.Windows.Forms.dll /reference:System.Drawing.dll Program.cs MainForm.cs
if errorlevel 1 exit /b %errorlevel%
if not exist WeaselUserDictSync.ini copy /y UserDictSync.ini.example WeaselUserDictSync.ini >nul
echo Built WeaselUserDictSync.exe
