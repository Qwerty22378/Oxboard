# Spike 3 player, run on sb-target: plays silence through WASAPI shared mode and logs what the
# app sees when its endpoint goes away.
#   player.ps1 -Id '{0.0.0.00000000}.{guid}' -Seconds 60 -Out C:\deploy\p1.log     (that endpoint)
#   player.ps1 -Default -Reopen -Seconds 80 -Out C:\deploy\p2.log                 (whatever Windows
#       gives this process as its default: follows a per-app route; reopens after a loss)
param([string]$Id = '', [switch]$Default, [switch]$Reopen, [int]$Seconds = 60, [int]$DelayS = 0, [int]$SegmentS = 0,
      [string]$Out = 'C:\deploy\player.log')

Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Threading;

[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDeviceEnumeratorCo {}

[Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceEnumerator {
    [PreserveSig] int EnumAudioEndpoints(int flow, int mask, out IntPtr devices);
    [PreserveSig] int GetDefaultAudioEndpoint(int flow, int role, out IMMDevice device);
    [PreserveSig] int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string id, out IMMDevice device);
}

[Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
    [PreserveSig] int Activate(ref Guid iid, int clsCtx, IntPtr p, [MarshalAs(UnmanagedType.IUnknown)] out object o);
    [PreserveSig] int OpenPropertyStore(int access, out IntPtr store);
    [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
}

[Guid("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioClient {
    [PreserveSig] int Initialize(int shareMode, int flags, long bufferDuration, long period, IntPtr format, IntPtr session);
    [PreserveSig] int GetBufferSize(out uint frames);
    [PreserveSig] int GetStreamLatency(out long latency);
    [PreserveSig] int GetCurrentPadding(out uint frames);
    [PreserveSig] int IsFormatSupported(int shareMode, IntPtr format, out IntPtr closest);
    [PreserveSig] int GetMixFormat(out IntPtr format);
    [PreserveSig] int GetDevicePeriod(out long def, out long min);
    [PreserveSig] int Start();
    [PreserveSig] int Stop();
    [PreserveSig] int Reset();
    [PreserveSig] int SetEventHandle(IntPtr h);
    [PreserveSig] int GetService(ref Guid iid, [MarshalAs(UnmanagedType.IUnknown)] out object o);
}

[Guid("F294ACFC-3146-4483-A7BF-ADDCA7C260E2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioRenderClient {
    [PreserveSig] int GetBuffer(uint frames, out IntPtr data);
    [PreserveSig] int ReleaseBuffer(uint frames, int flags);
}

public static class SpikePlayer {
    static Action<string> log;
    static DateTime t0;
    static void L(string s) { log(string.Format("{0,7:N2}s {1}", (DateTime.Now - t0).TotalSeconds, s)); }

    // One stream: open, play silence until the deadline or an error. Returns the error (0 = deadline).
    static int Play(string id, DateTime deadline) {
        var en = (IMMDeviceEnumerator)new MMDeviceEnumeratorCo();
        IMMDevice dev;
        int hr = id.Length > 0 ? en.GetDevice(id, out dev) : en.GetDefaultAudioEndpoint(0, 0, out dev);
        if (hr != 0) { L(string.Format("get device: 0x{0:X8}", hr)); return hr; }
        string got; dev.GetId(out got);
        object o; Guid iidClient = typeof(IAudioClient).GUID;
        hr = dev.Activate(ref iidClient, 23, IntPtr.Zero, out o);
        if (hr != 0) { L(string.Format("activate {0}: 0x{1:X8}", got, hr)); return hr; }
        var client = (IAudioClient)o;
        IntPtr fmt; client.GetMixFormat(out fmt);
        hr = client.Initialize(0, 0, 200000, 0, fmt, IntPtr.Zero);   // shared, 20 ms
        if (hr != 0) { L(string.Format("initialize: 0x{0:X8}", hr)); return hr; }
        uint size; client.GetBufferSize(out size);
        Guid iidRender = typeof(IAudioRenderClient).GUID;
        client.GetService(ref iidRender, out o);
        var render = (IAudioRenderClient)o;
        hr = client.Start();
        L(string.Format("playing on {0} (start 0x{1:X8})", got, hr));
        while (DateTime.Now < deadline) {
            uint pad;
            hr = client.GetCurrentPadding(out pad);
            if (hr == 0) {
                IntPtr data;
                uint n = size - pad;
                hr = render.GetBuffer(n, out data);
                if (hr == 0) hr = render.ReleaseBuffer(n, 2);   // AUDCLNT_BUFFERFLAGS_SILENT
            }
            if (hr != 0) { L(string.Format("stream error 0x{0:X8}", hr)); client.Stop(); return hr; }
            Thread.Sleep(5);
        }
        client.Stop();
        L("deadline: stopped");
        return 0;
    }

    public static void Run(string id, bool reopen, int seconds, int segment, Action<string> logTo) {
        log = logTo; t0 = DateTime.Now;
        var deadline = t0.AddSeconds(seconds);
        L(string.Format("pid {0}, target {1}", System.Diagnostics.Process.GetCurrentProcess().Id, id.Length > 0 ? id : "(default for this process)"));
        while (DateTime.Now < deadline) {
            var end = segment > 0 && DateTime.Now.AddSeconds(segment) < deadline ? DateTime.Now.AddSeconds(segment) : deadline;
            int hr = Play(id, end);
            if (hr != 0 && !reopen) return;
            if (DateTime.Now >= deadline) return;
            Thread.Sleep(1000);
            L("reopening");
        }
    }
}
'@

if ($DelayS -gt 0) { Start-Sleep -Seconds $DelayS }
"" | Set-Content $Out
[SpikePlayer]::Run($(if ($Default) { '' } else { $Id }), [bool]$Reopen, $Seconds, $SegmentS, [Action[string]]{ param($s) Add-Content -Path $Out -Value $s })
