<#
.SYNOPSIS
  Measure the V1 row against a board hosting its own network, on a laptop with
  one radio.

.DESCRIPTION
  Joining the board's access point costs this machine its internet, and with
  it any live session driving the work. So the whole measurement runs
  unattended: save the current network, join the board's, measure, write
  everything to disk, and come back. Nothing needs a person once it starts.

  Two outputs, on purpose. `v1-results.txt` is for a human to read.
  `v1-results.json` is for the session that was disconnected to parse when it
  returns -- every number, plus whether each step succeeded, so a failure is
  legible without guessing from prose.

  THE PASSPHRASE IS NEVER PRINTED. It is read from a gitignored file into a
  profile XML that is deleted as soon as the profile is added, and the profile
  itself is removed at the end so the key is not left in the machine's store.

.PARAMETER PreflightOnly
  Run every check that does NOT need the board's network, then stop. Do this
  FIRST, while still online: it confirms the firmware is flashed, the access
  point is up and the tools are present, so the offline window is not spent
  discovering something that could have been found with the internet on.

.PARAMETER Seconds
  Length of each streaming arm. Two arms plus a throughput pass run, so the
  offline window is roughly 3 * Seconds plus about 40 s of joining and leaving.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools\v1-offline.ps1 -PreflightOnly
  powershell -ExecutionPolicy Bypass -File tools\v1-offline.ps1 -Seconds 60
#>
[CmdletBinding()]
param(
    [switch]$PreflightOnly,
    [string]$ApSsid = "janus-cam",
    [string]$PassFile = "ap-pass.txt",
    [string]$BoardIp = "192.168.71.1",
    [string]$OutFile = "v1-results.txt",
    [string]$JsonFile = "v1-results.json",
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

# Everything the disconnected session needs, accumulated as we go so a crash
# still leaves a partial answer rather than nothing.
$R = [ordered]@{
    started_utc  = (Get-Date).ToUniversalTime().ToString("o")
    mode         = if ($PreflightOnly) { "preflight" } else { "measure" }
    seconds_per_arm = $Seconds
    ap_ssid      = $ApSsid
    board_ip     = $BoardIp
    steps        = [ordered]@{}
    arms         = @()
    throughput   = $null
    reconnected  = $false
    error        = $null
}

function Say($text) {
    $line = "[{0:HH:mm:ss}] {1}" -f (Get-Date), $text
    # Write-Host, never Write-Output: a Say inside a function that returns a
    # value would otherwise ride along in that value. On 2026-09-11 that made a
    # timed-out Wait-Until return @("gave up...", $false), which is truthy,
    # and the runner reported a join that never happened.
    Write-Host $line
    Add-Content -Path $OutFile -Value $line -Encoding utf8
}
function Record($text) { Add-Content -Path $OutFile -Value $text -Encoding utf8 }
function Step($name, $ok, $detail) {
    $R.steps[$name] = [ordered]@{ ok = [bool]$ok; detail = "$detail" }
    Say ("{0,-22} {1}  {2}" -f $name, $(if ($ok) { "ok  " } else { "FAIL" }), $detail)
}
function SaveJson {
    $R.finished_utc = (Get-Date).ToUniversalTime().ToString("o")
    $R | ConvertTo-Json -Depth 6 | Set-Content -Path $JsonFile -Encoding utf8
}

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
Record "Method line: board=$Board radio=softap-wpa2 geometry=320x240 format=jpeg"
Record "  fps_cap=15 metric=ffmpeg-decoded-frames arms=2 secs_per_arm=$Seconds"
Record "  laptop=one-radio-offline-batch oracle=ffmpeg+ffprobe"
Record ""

try {
    # ================= PRE-FLIGHT: everything that works online =============
    Step "ffmpeg" ($null -ne (Get-Command ffmpeg -ErrorAction SilentlyContinue)) "decoder and byte counter"
    Step "ffprobe" ($null -ne (Get-Command ffprobe -ErrorAction SilentlyContinue)) "geometry oracle"
    Step "espino" (Test-Path $Espino) $Espino

    $passOk = Test-Path $PassFile
    $passLen = 0
    if ($passOk) { $passLen = ((Get-Content -Path $PassFile -Raw).Trim()).Length }
    Step "passphrase-file" ($passOk -and $passLen -ge 8 -and $passLen -le 63) `
        "$PassFile, $passLen characters (8-63 needed; value never read aloud)"
    if (-not $R.steps["passphrase-file"].ok) {
        throw "write the access point's passphrase into $PassFile (8 to 63 characters)"
    }

    # ENCODING. PowerShell reads UTF-16 happily and bash's `$(cat ...)` does
    # not: it strips the null bytes and hands the build a different key than
    # the one this script will use to join. The firmware then refuses the
    # laptop and the offline trip is wasted on a wrong passphrase.
    #
    # This actually happened on 2026-09-08. `echo >` in PowerShell writes
    # UTF-16LE with a BOM by default, which is exactly the trap.
    $bytes = [IO.File]::ReadAllBytes((Resolve-Path $PassFile))
    $hasBom = $bytes.Length -ge 2 -and (($bytes[0] -eq 0xFF -and $bytes[1] -eq 0xFE) -or ($bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB))
    $hasNul = $bytes -contains 0
    $plain = (-not $hasBom) -and (-not $hasNul) -and ($bytes.Length -eq $passLen)
    Step "passphrase-encoding" $plain `
        $(if ($plain) { "plain UTF-8, $($bytes.Length) bytes for $passLen characters -- bash and PowerShell will agree" }
          else { "NOT plain UTF-8 (bom=$hasBom nul=$hasNul bytes=$($bytes.Length) chars=$passLen). The build and this script would use DIFFERENT keys. Rewrite the file as plain UTF-8 with no BOM and no trailing newline -- see docs/plans/offline-runs.md." })
    if (-not $plain) { throw "$PassFile is not plain UTF-8; the build would get a different passphrase than the join" }

    # Is the board actually running the AP firmware? Its own log says so, and
    # the serial link works with or without a network -- which is exactly why
    # this check belongs before the disconnection rather than after it.
    $seen = ""
    if (Test-Path $Espino) {
        $pre = "v1-preflight-serial.txt"
        $null = & $Espino monitor --port $SerialPort --board $Board --timeout 8 `
            2>&1 | Tee-Object -FilePath $pre
        $seen = (Get-Content $pre -Raw -ErrorAction SilentlyContinue)
    }
    $hosting = $seen -match "hosting\s+$([regex]::Escape($ApSsid))"
    $streaming = $seen -match "stream at http"
    Step "firmware-hosting" ($hosting -or $streaming) `
        $(if ($hosting) { "the board says it is hosting $ApSsid" }
          elseif ($streaming) { "the board is serving a stream" }
          else { "no 'hosting' line seen on $SerialPort in 6 s -- the board did not print its banner after a reset, so it is not running the hosting firmware" })

    # Can we see the network from here? ADVISORY, not fatal, and the reason is
    # that the instrument is unreliable on this machine: while connected, this
    # adapter reports exactly one SSID -- the one it is joined to -- across
    # repeated scans, in a home with several neighbours. One SSID is not a
    # plausible reading, so a negative here says nothing about the board.
    # Measured 2026-09-08 with the board provably hosting: `wifi:mode : softAP`
    # and a DHCP server on 192.168.71.1 in its own log, and still absent from
    # four consecutive scans.
    #
    # `netsh wlan connect` associates on its own rather than picking from this
    # cache, so the join below is the real test and this is a hint.
    $scan = (netsh wlan show networks mode=bssid) -join "`n"
    $visible = $scan -match [regex]::Escape($ApSsid)
    $ssidCount = ([regex]::Matches($scan, "(?m)^SSID \d+")).Count
    Step "ap-visible" $true `
        $(if ($visible) { "'$ApSsid' is in range" }
          elseif ($ssidCount -le 1) { "not in the scan, but the scan returned $ssidCount SSID(s) and is not to be believed -- advisory only, the join is the real test" }
          else { "'$ApSsid' not among $ssidCount SSIDs seen -- check the board is flashed with the AP build; proceeding anyway" })
    $R.ap_in_scan = $visible
    $R.ssids_in_scan = $ssidCount

    $saved = (netsh wlan show interfaces |
        Select-String -Pattern '^\s+Profile\s+:\s+(.+)$').Matches.Groups[1].Value.Trim()
    $R.saved_network = $saved
    Step "current-network" ($null -ne $saved) "'$saved' -- will return here"

    if ($PreflightOnly) {
        Say "pre-flight only; nothing was disconnected"
        $R.preflight_passed = $true
        return
    }

    # ================= the profile, then the disconnection ==================
    $psk = (Get-Content -Path $PassFile -Raw).Trim()
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
    $psk = $null; $xml = $null
    netsh wlan add profile filename="$profileXml" user=current | Out-Null
    $addedProfile = $true
    Remove-Item -Path $profileXml -Force -ErrorAction SilentlyContinue
    $profileXml = $null
    Step "profile-added" $true "key not shown, temp file deleted"

    # The board's own frame count is the self-metric; ffmpeg's is the oracle.
    # Both are wanted, and serial works with or without a network.
    if (Test-Path $Espino) {
        $monitor = Start-Process -FilePath $Espino `
            -ArgumentList @("monitor", "--port", $SerialPort, "--board", $Board, "--timeout", "$($Seconds * 3 + 150)") `
            -RedirectStandardOutput "v1-serial.txt" -RedirectStandardError "v1-serial.err.txt" `
            -PassThru -WindowStyle Hidden
        Say "watching the board on $SerialPort (reset first: nobody is connected yet, and the banner proves it is hosting)"
        Start-Sleep -Seconds 3
    }

    Say "joining '$ApSsid' -- the session driving this is now out of contact"
    netsh wlan disconnect interface="$iface" 2>&1 | Out-Null
    $landed = ""
    $assoc = $false
    foreach ($attempt in 1..6) {
        Start-Sleep -Seconds 6        # let the adapter scan after the disconnect
        netsh wlan connect name="$ApSsid" ssid="$ApSsid" interface="$iface" 2>&1 | Out-Null
        Start-Sleep -Seconds 5
        $ifc = netsh wlan show interfaces | Out-String
        $landed = [regex]::Match($ifc, '(?m)^\s*SSID\s*:\s*(.+?)\s*$').Groups[1].Value
        $state = [regex]::Match($ifc, '(?m)^\s*State\s*:\s*(.+?)\s*$').Groups[1].Value
        if ($landed -eq $ApSsid -and $state -match 'connected') { $assoc = $true; break }
        Say "attempt $attempt`: on '$landed' ($state), not '$ApSsid' -- retrying"
    }
    Step "associated" $assoc $(if ($assoc) { "on '$ApSsid' after $attempt attempt(s)" } else { "landed on '$landed' -- the adapter never found '$ApSsid'; Windows fell back" })
    if (-not $assoc) {
        Record (netsh wlan show interfaces | Out-String)
        Record (netsh wlan show networks mode=bssid | Out-String)
        throw "never associated with $ApSsid (on '$landed' instead)"
    }
    # What the link actually is, while we are on it: radio type, channel,
    # signal. The next failure should explain itself from this file alone.
    Record "== link, as Windows sees it =="
    Record ((netsh wlan show interfaces | Out-String) -split "`n" | Where-Object { $_ -match 'SSID|State|Radio type|Channel|Signal|Band|Authentication' } | Out-String)

    $joined = Wait-Until {
        Test-NetConnection -ComputerName $BoardIp -Port 80 -InformationLevel Quiet -WarningAction SilentlyContinue
    } 45 "the board to answer on $BoardIp`:80"
    Step "joined" $joined "$BoardIp:80"
    if (-not $joined) {
        Record (netsh wlan show interfaces | Out-String)
        throw "could not reach $BoardIp after joining $ApSsid"
    }

    $addr = (Get-NetIPAddress -InterfaceAlias $iface -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        ForEach-Object IPAddress) -join ", "
    $R.laptop_ip = $addr
    Record "== addresses on the interface =="
    Record ((ipconfig /all | Out-String) -split "`n" | Where-Object { $_ -match 'IPv4|DHCP Server|Lease|Default Gateway|Subnet' } | Out-String)
    Record "laptop address on the board's network: $addr"

    $ping = Test-Connection -ComputerName $BoardIp -Count 10 -ErrorAction SilentlyContinue
    if ($ping) {
        $rtt = $ping | Measure-Object -Property ResponseTime -Average -Minimum -Maximum
        $R.rtt_ms = [ordered]@{ min = $rtt.Minimum; mean = [math]::Round($rtt.Average, 2); max = $rtt.Maximum }
        Record ("link RTT over 10 pings: min {0} ms, mean {1:N1} ms, max {2} ms" -f $rtt.Minimum, $rtt.Average, $rtt.Maximum)
    }
    Record ""

    # ================= what the stream says it is ===========================
    $url = "http://$BoardIp/stream"
    Record "== ffprobe on $url =="
    $probe = (& ffprobe -hide_banner -v error -show_entries `
        "stream=codec_name,width,height,pix_fmt" -of "default=noprint_wrappers=1" $url 2>&1 | Out-String).Trim()
    Record $probe
    $R.ffprobe = $probe
    Step "ffprobe" ($probe -match "codec_name") "geometry read from the bitstream"
    Record ""

    # ================= two decode arms ======================================
    foreach ($arm in 1, 2) {
        Say "arm $arm of 2: $Seconds s of $url"
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $err = & ffmpeg -hide_banner -nostats -i $url -t $Seconds -f null - 2>&1 | Out-String
        $sw.Stop()
        $frames = 0
        $m = [regex]::Matches($err, 'frame=\s*(\d+)')
        if ($m.Count -gt 0) { $frames = [int]$m[$m.Count - 1].Groups[1].Value }
        $secs = $sw.Elapsed.TotalSeconds
        $fps = if ($secs -gt 0) { [math]::Round($frames / $secs, 3) } else { 0 }
        $R.arms += [ordered]@{ arm = $arm; frames = $frames; wall_seconds = [math]::Round($secs, 3); fps = $fps }
        Record "== arm $arm =="
        Record ("frames decoded : {0}" -f $frames)
        Record ("wall seconds   : {0:N3}" -f $secs)
        Record ("frames per sec : {0:N3}" -f $fps)
        Record "ffmpeg's last lines:"
        Record ((($err -split "`n") | Select-Object -Last 6) -join "`n").Trim()
        Record ""
        Step "arm$arm" ($frames -gt 0) "$frames frames, $fps fps"
    }

    # ================= throughput, its own request ==========================
    # A frame count and a byte count are different questions, so this takes its
    # own pass rather than being a by-product of a decode loop.
    Say "measuring bytes off the link for $Seconds s"
    $raw = "v1-raw.mjpeg"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & ffmpeg -hide_banner -nostats -v error -i $url -t $Seconds -c copy -f mpjpeg $raw -y 2>&1 | Out-Null
    $sw.Stop()
    if (Test-Path $raw) {
        $bytes = (Get-Item $raw).Length
        $secs = $sw.Elapsed.TotalSeconds
        $R.throughput = [ordered]@{
            bytes = $bytes
            seconds = [math]::Round($secs, 3)
            kib_per_s = [math]::Round($bytes / 1024 / $secs, 1)
            mbit_per_s = [math]::Round($bytes * 8 / 1e6 / $secs, 3)
        }
        Record "== throughput =="
        Record ("bytes over the link : {0}" -f $bytes)
        Record ("kilobytes / second  : {0:N1}" -f ($bytes / 1024 / $secs))
        Record ("megabits / second   : {0:N3}" -f ($bytes * 8 / 1e6 / $secs))
        Remove-Item $raw -Force -ErrorAction SilentlyContinue
        Step "throughput" $true ("{0:N1} KiB/s" -f ($bytes / 1024 / $secs))
    } else {
        Step "throughput" $false "no bytes captured"
    }
    Record ""
    Say "measurement finished"
}
catch {
    $R.error = $_.Exception.Message
    Record ""
    Record "ERROR: $($_.Exception.Message)"
    Say "failed: $($_.Exception.Message)"
}
finally {
    if ($monitor -and -not $monitor.HasExited) {
        Stop-Process -Id $monitor.Id -Force -ErrorAction SilentlyContinue
    }
    if ($profileXml -and (Test-Path $profileXml)) {
        Remove-Item -Path $profileXml -Force -ErrorAction SilentlyContinue
    }
    # The board's own counters, for the ledger's self-metric column.
    if (Test-Path "v1-serial.txt") {
        $tail = (Get-Content "v1-serial.txt" -Tail 12 -ErrorAction SilentlyContinue) -join "`n"
        $R.board_serial_tail = $tail
        Record "== what the board said about itself =="
        Record $tail
    }
    if ($saved -and -not $PreflightOnly) {
        Say "returning to '$saved'"
        netsh wlan connect name="$saved" interface="$iface" 2>&1 | Out-Null
        $R.reconnected = Wait-Until {
            Test-NetConnection -ComputerName "1.1.1.1" -Port 443 -InformationLevel Quiet -WarningAction SilentlyContinue
        } 60 "the internet to come back"
        if ($R.reconnected) { Say "back on '$saved' with internet" }
        else { Say "NOT back online -- reconnect from the Wi-Fi picker" }
    }
    if ($addedProfile) {
        netsh wlan delete profile name="$ApSsid" 2>&1 | Out-Null
        Say "removed the '$ApSsid' profile so the key is not kept"
    }
    SaveJson
    Say "results in $OutFile and $JsonFile"
}
