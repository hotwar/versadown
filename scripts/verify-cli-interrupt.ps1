param(
    [Parameter(Mandatory)][string]$CliBinary,
    [Parameter(Mandatory)][string]$Discovery,
    [Parameter(Mandatory)][string]$TaskId,
    [Parameter(Mandatory)][string]$ResultDirectory,
    [switch]$InheritIgnore
)
$ErrorActionPreference = 'Stop'

# A redirected stdin byte is not a Windows console event. Create a hidden,
# private console for just this client, attach this helper to that console,
# then broadcast CTRL_C_EVENT there. Never attach to the caller's console.
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class CliInterruptProbe {
    [StructLayout(LayoutKind.Sequential)]
    struct SecurityAttributes {
        public int length;
        public IntPtr descriptor;
        [MarshalAs(UnmanagedType.Bool)] public bool inherit;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct StartupInfo {
        public int cb;
        public string reserved, desktop, title;
        public int x, y, width, height, xChars, yChars, fill, flags;
        public short show, reservedSize;
        public IntPtr reservedPtr, stdin, stdout, stderr;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ProcessInfo {
        public IntPtr process, thread;
        public uint processId, threadId;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string application, StringBuilder commandLine,
        IntPtr processAttributes, IntPtr threadAttributes, bool inheritHandles,
        uint flags, IntPtr environment, string directory, ref StartupInfo startup,
        out ProcessInfo process);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFileW(string file, uint access, uint share,
        ref SecurityAttributes security, uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AttachConsole(uint processId);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetConsoleCtrlHandler(IntPtr handler, bool add);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GenerateConsoleCtrlEvent(uint controlEvent, uint processGroup);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr handle, uint timeout);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateProcess(IntPtr process, uint exitCode);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    static void Check(bool ok, string operation) {
        if (!ok) throw new Win32Exception(Marshal.GetLastWin32Error(), operation);
    }
    static string Quote(string value) {
        var output = new StringBuilder("\"");
        int slashes = 0;
        foreach (char ch in value) {
            if (ch == '\\') { slashes++; continue; }
            if (ch == '"') { output.Append('\\', slashes * 2 + 1); output.Append(ch); }
            else { output.Append('\\', slashes); output.Append(ch); }
            slashes = 0;
        }
        output.Append('\\', slashes * 2);
        output.Append('"');
        return output.ToString();
    }
    static IntPtr File(string path, bool input) {
        var security = new SecurityAttributes {
            length = Marshal.SizeOf(typeof(SecurityAttributes)), inherit = true
        };
        IntPtr handle = CreateFileW(path, input ? 0x80000000u : 0x40000000u,
            3, ref security, input ? 3u : 2u, 0x80, IntPtr.Zero);
        Check(handle != new IntPtr(-1), "Create private test stream");
        return handle;
    }
    public static uint Run(string binary, string discovery, string taskId, string output, string error, bool inheritIgnore) {
        IntPtr stdin = IntPtr.Zero, stdout = IntPtr.Zero, stderr = IntPtr.Zero;
        ProcessInfo child = new ProcessInfo();
        bool attached = false;
        try {
            stdin = File("NUL", true); stdout = File(output, false); stderr = File(error, false);
            var startup = new StartupInfo {
                cb = Marshal.SizeOf(typeof(StartupInfo)), flags = 0x101, show = 0,
                stdin = stdin, stdout = stdout, stderr = stderr
            };
            string command = Quote(binary) + " --json --lang en --discovery " + Quote(discovery)
                + " --timeout 20 wait " + Quote(taskId);
            Check(SetConsoleCtrlHandler(IntPtr.Zero, inheritIgnore), "Set inherited Ctrl-C behavior");
            // CREATE_NEW_CONSOLE; SW_HIDE keeps the dedicated test console hidden.
            Check(CreateProcessW(binary, new StringBuilder(command), IntPtr.Zero, IntPtr.Zero,
                true, 0x10, IntPtr.Zero, null, ref startup, out child), "Create owned CLI console");
            // Allow the asynchronous Ctrl-C listener to register. The assertions
            // below require the client's JSON INTERRUPTED response, so default
            // Windows termination cannot accidentally satisfy this test.
            Thread.Sleep(1000);
            Check(WaitForSingleObject(child.process, 0) == 258, "CLI exited before Ctrl-C");
            FreeConsole();
            Check(AttachConsole(child.processId), "Attach to owned CLI console");
            attached = true;
            Check(SetConsoleCtrlHandler(IntPtr.Zero, true), "Ignore helper Ctrl-C");
            Check(GenerateConsoleCtrlEvent(0, 0), "Send CTRL_C_EVENT to private console");
            Check(WaitForSingleObject(child.process, 10000) == 0, "CLI did not handle Ctrl-C promptly");
            uint exitCode;
            Check(GetExitCodeProcess(child.process, out exitCode), "Read CLI exit code");
            return exitCode;
        } finally {
            if (attached) FreeConsole();
            if (child.process != IntPtr.Zero) {
                // Failure-only cleanup of the process created above. Terminating
                // it is never accepted as evidence of successful Ctrl-C handling.
                if (WaitForSingleObject(child.process, 0) == 258) {
                    TerminateProcess(child.process, 1);
                    WaitForSingleObject(child.process, 5000);
                }
                CloseHandle(child.process); CloseHandle(child.thread);
            }
            foreach (IntPtr handle in new[] { stdin, stdout, stderr })
                if (handle != IntPtr.Zero && handle != new IntPtr(-1)) CloseHandle(handle);
        }
    }
}
'@

$stdoutPath = Join-Path $ResultDirectory 'ctrl-c.stdout.json'
$stderrPath = Join-Path $ResultDirectory 'ctrl-c.stderr.log'
$exitCode = [CliInterruptProbe]::Run($CliBinary, $Discovery, $TaskId, $stdoutPath, $stderrPath, $InheritIgnore.IsPresent)
$response = Get-Content -LiteralPath $stdoutPath -Raw | ConvertFrom-Json
if ($exitCode -ne 130 -or $response.ok -ne $false -or $response.error.code -ne 'INTERRUPTED') {
    throw "CLI did not return INTERRUPTED with exit code 130 (actual exit: $exitCode)."
}
@{ ok = $true; exit_code = $exitCode; code = $response.error.code; method = 'GenerateConsoleCtrlEvent'; private_console = $true } | ConvertTo-Json -Compress
