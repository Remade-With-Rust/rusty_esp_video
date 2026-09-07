<#
.SYNOPSIS
  Measure the V1 row against a board that is hosting its own network, on a
  laptop that has only one radio.

.DESCRIPTION
  Joining the board's access point costs this machine its internet, and with
  it any live session driving the work. So the whole measurement runs
  unattended: save the current network, join the board's, measure, write
  everything to a file, and come back. Nothing here needs a person once it
  starts, and the results are on disk whether or not anyone was watching.

  The reconnect is in a finally block, so it happens even when the
  measurement throws. If this script is killed outright, the Wi-Fi picker is
  the backstop.

  THE PASSPHRASE IS NEVER PRINTED. It is read from a gitignored file into a
  profile XML that is written with the file's own permissions and deleted in
  the same finally block, and the profile itself is removed at the end so the
  key is not left in the machine's store.

.PARAMETER Seconds
  Length of each streaming arm. Two arms run, so the offline window is
  roughly 2 * Seconds plus about 40 s of joining and leaving.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools\v1-offline.ps1 -Seconds 60
#>
[CmdletBinding()]
param(
    [string]$ApSsid = "janus-cam",
    [string]$PassFile = "ap-pass.txt",
    [string]$BoardIp = "192.168.71.1",
    [string]$OutFile = "v1-results.txt",
    [int]$Seconds = 60,
    [string]$SerialPort = "COM4",
    [string]$Board = "xiao-esp32s3-sense",
    [string]$Espino = "C:/janus-e/release/espino.exe"
)

$ErrorActionPreference = "Stop"
$iface = "Wi-Fi"
$profileXml = $null
$addedProfile = $false
$saved = $null
$monitor = $null

function Say($text) {
    $line = "[{0:HH:mm:ss}] {1}" -f (Get-Date), $text
    Write-Output $line
    Add-Content -Path $OutFile -Value $line -Encoding utf8
}

function Record($text) {
    Add-Content -Path $OutFile -Value $text -Encoding utf8
}

# Wait until $test returns true, or give up. Returns whether it came true.
function Wait-Until([scriptblock]$test, [int]$timeoutSec, [string]$what) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        try { if (& $test) { return $true } } catch { }
        Start-Sleep -Milliseconds 700
    }
    Say "gave up waiting for $what after $timeoutSec s"
    return $false
}

Set-Content -Path $OutFile -Value "" -Encoding utf8
Record "V1: MJPEG over the board's own access point"
Record "Method line: board=xiao-esp32s3-sense radio=softap-wpa2 geometry=320x240"
Record "  format=jpeg fps_cap=15 metric=ffmpeg-decoded-frames arms=2 secs_per_arm=$Seconds"
Record "  laptop=one-radio-offline-batch oracle=ffmpeg+ffprobe"
Record ""

try {
    # --- where we are now, so we can come back -----------------------------
    $saved = (netsh wlan show interfaces |
        Select-String -Pattern '^\s+Profile\s+:\s+(.+)$').Matches.Groups[1].Value.Trim()
    Say "current network is '$saved'; will return here"

    # --- a profile for the board's network ---------------------------------
    if (-not (Test-Path $PassFile)) {
        throw "no $PassFile. Write the access point's passphrase into it (8 to 63 characters); it is gitignored and never printed."
    }
    $psk = (Get-Content -Path $PassFile -Raw).Trim()
    if ($psk.Length -lt 8 -or $psk.Length -gt 63) {
        throw "the passphrase in $PassFile must be 8 to 63 characters (it is $($psk.Length))"
    }

    $profileXml = Join-Path $env:TEMP "janus-ap-$PID.xml"
    $xml = @"
<?xml version="1.0"?>
<WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1">
  <name>$ApSsid</name>
  <SSIDConfig><SSID><name>$ApSsid</name></SSID></SSIDConfig>
  <connectionType>ESS</connectionType>
  <connectionMode>manual</connectionMode>
  <MSM><security>
    <authEncryption>
      <authentication>WPA2PSK</authentication>
      <encryption>AES</encryption>
      <useOneX>false</useOneX>
    </authEncryption>
    <sharedKey>
      <keyType>passPhrase</keyType>
      <protected>false</protected>
      <keyMaterial>$psk</keyMaterial>
    </sharedKey>
  </security></MSM>
</WLANProfile>
"@
    Set-Content -Path $profileXml -Value $xml -Encoding utf8
    $psk = $null
    netsh wlan add profile filename="$profileXml" user=current | Out-Null
    $addedProfile = $true
    Remove-Item -Path $profileXml -Force -ErrorAction SilentlyContinue
    $profileXml = $null
    Say "added a profile for '$ApSsid' (key not shown, temp file deleted)"

    # --- watch the board's own side of the story ---------------------------
    # Its frame count is the self-metric; ffmpeg's is the oracle. Both are
    # wanted, and the serial link works whether or not there is a network.
    $serialLog = "v1-serial.txt"
    if (Test-Path $Espino) {
        $monitor = Start-Process -FilePath $Espino `
            -ArgumentList @("monitor", "--port", $SerialPort, "--board", $Board, "--no-reset", "--timeout", "$($Seconds * 2 + 120)") `
            -RedirectStandardOutput $serialLog -RedirectStandardError "v1-serial.err.txt" `
            -PassThru -WindowStyle Hidden
        Say "watching the board on $SerialPort (no reset, so the stream is not interrupted)"
    } else {
        Say "no espino at $Espino; the board's own counters will not be captured"
    }

    # --- leave the internet ------------------------------------------------
    Say "joining '$ApSsid' -- the session driving this is now out of contact"
    netsh wlan connect name="$ApSsid" ssid="$ApSsid" interface="$iface" | Out-Null

    $joined = Wait-Until {
        (Test-NetConnection -ComputerName $BoardIp -Port 80 -InformationLevel Quiet -WarningAction SilentlyContinue)
    } 45 "the board to answer on $BoardIp`:80"

    if (-not $joined) {
        Record "FAILED to reach the board. What the adapter says:"
        Record (netsh wlan show interfaces | Out-String)
        throw "could not reach $BoardIp after joining $ApSsid"
    }
    Say "joined; the board answers on $BoardIp"

    $addr = (Get-NetIPAddress -InterfaceAlias $iface -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Select-Object -First 1).IPAddress
    Record "laptop address on the board's network: $addr"

    $ping = Test-Connection -ComputerName $BoardIp -Count 10 -ErrorAction SilentlyContinue
    if ($ping) {
        $rtt = ($ping | Measure-Object -Property ResponseTime -Average -Minimum -Maximum)
        Record ("link RTT over 10 pings: min {0} ms, mean {1:N1} ms, max {2} ms" -f `
            $rtt.Minimum, $rtt.Average, $rtt.Maximum)
    }
    Record ""

    # --- what the stream says it is ----------------------------------------
    $url = "http://$BoardIp/stream"
    Record "== ffprobe on $url =="
    $probe = & ffprobe -hide_banner -v error -show_entries `
        "stream=codec_name,width,height,pix_fmt" -of "default=noprint_wrappers=1" $url 2>&1
    Record ($probe | Out-String).Trim()
    Record ""

    # --- two arms, each decoded to completion ------------------------------
    foreach ($arm in 1, 2) {
        Say "arm $arm of 2: $Seconds s of $url"
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $err = & ffmpeg -hide_banner -nostats -i $url -t $Seconds -f null - 2>&1 | Out-String
        $sw.Stop()

        # ffmpeg's own summary line carries the frame count and the rate it
        # decoded them at; the wall clock is the cross-check on it.
        $frames = 0
        $m = [regex]::Matches($err, 'frame=\s*(\d+)')
        if ($m.Count -gt 0) { $frames = [int]$m[$m.Count - 1].Groups[1].Value }
        $secs = $sw.Elapsed.TotalSeconds
        $fps = if ($secs -gt 0) { $frames / $secs } else { 0 }

        Record "== arm $arm =="
        Record ("frames decoded : {0}" -f $frames)
        Record ("wall seconds   : {0:N3}" -f $secs)
        Record ("frames per sec : {0:N3}" -f $fps)
        $bitrate = [regex]::Match($err, 'bitrate=\s*([0-9.]+\s*\w+/s)')
        if ($bitrate.Success) { Record ("ffmpeg bitrate : {0}" -f $bitrate.Groups[1].Value) }
        $speed = [regex]::Match($err, 'speed=\s*([0-9.]+x)')
        if ($speed.Success) { Record ("ffmpeg speed   : {0}" -f $speed.Groups[1].Value) }
        Record "ffmpeg's last lines:"
        Record (($err -split "`n" | Select-Object -Last 6) -join "`n").Trim()
        Record ""
    }

    # --- throughput, separately from the frame rate ------------------------
    # A frame count and a byte count are different questions; this one is the
    # link's, and it is taken with its own request rather than shared with
    # the decode loop.
    Say "measuring bytes off the link for $Seconds s"
    $raw = "v1-raw.mjpeg"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & ffmpeg -hide_banner -nostats -v error -i $url -t $Seconds -c copy -f mpjpeg $raw -y 2>&1 | Out-Null
    $sw.Stop()
    if (Test-Path $raw) {
        $bytes = (Get-Item $raw).Length
        $secs = $sw.Elapsed.TotalSeconds
        Record "== throughput =="
        Record ("bytes over the link : {0}" -f $bytes)
        Record ("seconds             : {0:N3}" -f $secs)
        Record ("kilobytes / second  : {0:N1}" -f ($bytes / 1024 / $secs))
        Record ("megabits / second   : {0:N3}" -f ($bytes * 8 / 1e6 / $secs))
        Remove-Item $raw -Force -ErrorAction SilentlyContinue
    }
    Record ""
    Say "measurement finished"
}
catch {
    Record ""
    Record "ERROR: $($_.Exception.Message)"
    Say "failed: $($_.Exception.Message)"
}
finally {
    # Always come back, whatever happened above. This is the block that keeps
    # an unattended run from stranding the machine on a camera's network.
    if ($monitor -and -not $monitor.HasExited) {
        Stop-Process -Id $monitor.Id -Force -ErrorAction SilentlyContinue
    }
    if ($profileXml -and (Test-Path $profileXml)) {
        Remove-Item -Path $profileXml -Force -ErrorAction SilentlyContinue
    }
    if ($saved) {
        Say "returning to '$saved'"
        netsh wlan connect name="$saved" interface="$iface" 2>&1 | Out-Null
        $back = Wait-Until {
            (Test-NetConnection -ComputerName "1.1.1.1" -Port 443 -InformationLevel Quiet -WarningAction SilentlyContinue)
        } 60 "the internet to come back"
        if ($back) { Say "back on '$saved' with internet" }
        else { Say "NOT back online -- reconnect from the Wi-Fi picker" }
    }
    if ($addedProfile) {
        # do not leave the key sitting in the machine's profile store
        netsh wlan delete profile name="$ApSsid" 2>&1 | Out-Null
        Say "removed the '$ApSsid' profile so the key is not kept"
    }
    Say "results in $OutFile"
}
