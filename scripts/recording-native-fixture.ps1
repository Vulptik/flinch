param([ValidateRange(30, 1200)][int]$Seconds = 660)
# Explicit desktop fixture. Owns one temporary window and a looped PCM tone buffer.
# No browser, network, global input, clipboard or user preference changes.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Windows.Forms,System.Drawing -TypeDefinition @"
using System;
using System.Diagnostics;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Windows.Forms;
public class FlinchSyncFixture : Form {
 [StructLayout(LayoutKind.Sequential)] struct Format { public ushort tag, channels; public uint rate, average; public ushort block, bits, extra; }
 [StructLayout(LayoutKind.Sequential)] struct Header { public IntPtr data; public uint length, recorded; public IntPtr user; public uint flags, loops; public IntPtr next, reserved; }
 [StructLayout(LayoutKind.Explicit, Size=12)] struct Position { [FieldOffset(0)] public uint type; [FieldOffset(4)] public uint value; }
 [DllImport("winmm.dll")] static extern uint waveOutOpen(out IntPtr h, uint device, ref Format f, IntPtr callback, IntPtr instance, uint flags);
 [DllImport("winmm.dll")] static extern uint waveOutPrepareHeader(IntPtr h, IntPtr header, uint size);
 [DllImport("winmm.dll")] static extern uint waveOutWrite(IntPtr h, IntPtr header, uint size);
 [DllImport("winmm.dll")] static extern uint waveOutGetPosition(IntPtr h, ref Position p, uint size);
 [DllImport("winmm.dll")] static extern uint waveOutReset(IntPtr h);
 [DllImport("winmm.dll")] static extern uint waveOutUnprepareHeader(IntPtr h, IntPtr header, uint size);
 [DllImport("winmm.dll")] static extern uint waveOutClose(IntPtr h);
 [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
 IntPtr device, buffer, header;
 [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern IntPtr CreateWaitableTimerEx(IntPtr attributes, string name, uint flags, uint access);
 [DllImport("kernel32.dll")] static extern bool SetWaitableTimer(IntPtr timer, ref long due, int period, IntPtr routine, IntPtr arg, bool resume);
 [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint timeout);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
 System.Threading.Thread painter;
 volatile bool stopped;
 Stopwatch clock = Stopwatch.StartNew();
 int seconds; double position;
 static void Check(uint code) { if(code != 0) throw new Exception("waveOut failed: " + code); }
 public FlinchSyncFixture(int duration) {
  SetProcessDPIAware(); seconds = duration; DoubleBuffered = true; TopMost = true;
  Text = "Flinch recording acceptance - closes automatically"; FormBorderStyle=FormBorderStyle.None;
  StartPosition=FormStartPosition.Manual; Location=new Point(0,0); ClientSize=new Size(1280,720);
  Format f = new Format { tag=1, channels=2, rate=48000, average=192000, block=4, bits=16 };
  Check(waveOutOpen(out device, 0xffffffff, ref f, IntPtr.Zero, IntPtr.Zero, 0));
  // 2 s loop, 100 ms 900 Hz tone at 1 s, quiet amplitude.
  byte[] pcm=new byte[384000];
  for(int n=48000;n<52800;n++) { short v=(short)(6000*Math.Sin(n*2*Math.PI*900/48000));
   for(int c=0;c<2;c++) { pcm[n*4+c*2]=(byte)v; pcm[n*4+c*2+1]=(byte)(v>>8); } }
  buffer=Marshal.AllocHGlobal(pcm.Length); Marshal.Copy(pcm,0,buffer,pcm.Length);
  Header h = new Header { data=buffer,length=(uint)pcm.Length, flags=12,loops=0xffffffff };
  header=Marshal.AllocHGlobal(Marshal.SizeOf(typeof(Header))); Marshal.StructureToPtr(h,header,false);
  Check(waveOutPrepareHeader(device,header,(uint)Marshal.SizeOf(typeof(Header))));
  Check(waveOutWrite(device,header,(uint)Marshal.SizeOf(typeof(Header))));
  Shown += delegate { painter = new System.Threading.Thread(delegate() {
   IntPtr pacing=CreateWaitableTimerEx(IntPtr.Zero,null,2,0x100002);
   if(pacing==IntPtr.Zero) throw new Exception("Fixture frame timer unavailable");
   try { while(!stopped) {
    long due=-80000;
    if(!SetWaitableTimer(pacing,ref due,0,IntPtr.Zero,IntPtr.Zero,false) || WaitForSingleObject(pacing,1000)!=0) throw new Exception("Fixture pacing failed");
    if(!stopped) BeginInvoke((Action)delegate { if(stopped) return; if(clock.Elapsed.TotalSeconds>seconds) { Close(); return; }
   Position p=new Position {type=2}; Check(waveOutGetPosition(device,ref p,12));
   position=p.type==2 ? p.value/48000.0 : p.type==1 ? p.value/1000.0 : p.value/192000.0;
   Invalidate(); Update();
    });
   }} finally { CloseHandle(pacing); }
  }); painter.IsBackground=true; painter.Start(); };
 }
 protected override void OnPaint(PaintEventArgs e) {
  double phase=position%2; bool pulse=phase>=1 && phase<1.1;
  e.Graphics.Clear(Color.FromArgb(8,8,8));
  e.Graphics.FillRectangle(pulse?Brushes.White:Brushes.Black,32,32,128,128);
  int frame=(int)(clock.Elapsed.TotalSeconds*120);
  for(int bit=0;bit<16;bit++) e.Graphics.FillRectangle(((frame>>bit)&1)!=0?Brushes.Lime:Brushes.DarkGreen,32+bit*64,220,48,120);
  e.Graphics.FillRectangle(Brushes.DodgerBlue,(frame*7)%1150,400,90,160);
  using(Font font=new Font("Segoe UI",22)) e.Graphics.DrawString("Flinch desktop/audio acceptance   "+position.ToString("F3")+" s",font,Brushes.White,32,620);
 }
 protected override void Dispose(bool disposing) {
  stopped=true; if(painter!=null && !painter.Join(1500)) throw new Exception("Fixture paint thread did not stop");
  if(device!=IntPtr.Zero) { waveOutReset(device); waveOutUnprepareHeader(device,header,(uint)Marshal.SizeOf(typeof(Header))); waveOutClose(device); device=IntPtr.Zero; }
  if(header!=IntPtr.Zero) { Marshal.FreeHGlobal(header); header=IntPtr.Zero; }
  if(buffer!=IntPtr.Zero) { Marshal.FreeHGlobal(buffer); buffer=IntPtr.Zero; }
  base.Dispose(disposing);
 }
 public static void Run(int duration) { Application.Run(new FlinchSyncFixture(duration)); }
}
"@
[FlinchSyncFixture]::Run($Seconds)
