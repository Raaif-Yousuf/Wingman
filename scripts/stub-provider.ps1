<#
.SYNOPSIS
  A stand-in for Ollama, for the install smoke test (#128). It listens on
  127.0.0.1 only, on a port the operating system picks, answers the three
  calls Wingman makes in Local mode, and records every request so a script
  can check the app really asked.

.DESCRIPTION
  Dot-source this file; it only defines functions.

    Start-StubProvider    starts it, returns an object with .Port and .Requests
    Stop-StubProvider     stops it
    Get-StubResponse      pure: what it answers for a method and path

  No key, no network beyond the loopback, no model. The answer is a fixed
  headline and detail, so a script can look for them on the card.
#>

$StubModel = 'gemma3:4b'
$StubHeadline = 'Stub provider answer'
$StubDetail = 'This answer came from the install smoke stub, not a model.'

# What the stub answers for a request. Pure. Returns Status and Body (JSON).
#   GET  /api/tags   the model list the Local mode probe reads
#   POST /api/chat   one whole answer in the shape Wingman asks for
#   anything else    404
function Get-StubResponse {
    param(
        [Parameter(Mandatory)] [string] $Method,
        [Parameter(Mandatory)] [string] $Path
    )
    $route = ($Path -split '\?')[0]
    if ($Method -eq 'GET' -and $route -eq '/api/tags') {
        $body = @{ models = @(@{ name = $StubModel; model = $StubModel }) } | ConvertTo-Json -Compress -Depth 5
        return [pscustomobject]@{ Status = 200; Body = $body }
    }
    if ($Method -eq 'POST' -and $route -eq '/api/chat') {
        $answer = [ordered]@{ detail = $StubDetail; headline = $StubHeadline; difficulty = '3' } | ConvertTo-Json -Compress
        $body = [ordered]@{
            model       = $StubModel
            message     = [ordered]@{ role = 'assistant'; content = $answer }
            done        = $true
            done_reason = 'stop'
        } | ConvertTo-Json -Compress -Depth 5
        return [pscustomobject]@{ Status = 200; Body = $body }
    }
    [pscustomobject]@{ Status = 404; Body = '{"error":"not found"}' }
}

# The server loop, run on its own runspace so the caller stays free. Plain
# HTTP/1.1 over a TcpListener: one request per connection, Connection: close.
# It never reads a file and never connects out.
$script:StubServerLoop = {
    param($listener, $requests, $stop, $respond)
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    while (-not $stop.Value) {
        $client = $null
        try {
            if (-not $listener.Pending()) { Start-Sleep -Milliseconds 10; continue }
            $client = $listener.AcceptTcpClient()
            $client.ReceiveTimeout = 5000
            $s = $client.GetStream()
            $head = New-Object System.Text.StringBuilder
            $buf = New-Object byte[] 1
            while (-not $head.ToString().EndsWith("`r`n`r`n")) {
                if ($s.Read($buf, 0, 1) -lt 1) { break }
                [void]$head.Append([char]$buf[0])
                if ($head.Length -gt 65536) { break }
            }
            $lines = $head.ToString() -split "`r`n"
            $first = $lines[0] -split ' '
            $length = 0
            foreach ($l in $lines) { if ($l -match '^(?i)content-length:\s*(\d+)') { $length = [int]$Matches[1] } }
            $body = New-Object byte[] $length
            $got = 0
            while ($got -lt $length) {
                $n = $s.Read($body, $got, $length - $got)
                if ($n -lt 1) { break }
                $got += $n
            }
            [void]$requests.Add([pscustomobject]@{
                    Method = $first[0]
                    Path   = $first[1]
                    Bytes  = $got
                    # Latin-1 keeps every byte, so a search for text in the body is exact.
                    Body   = $latin1.GetString($body, 0, $got)
                })
            $r = & $respond $first[0] $first[1]
            $payload = [System.Text.Encoding]::UTF8.GetBytes($r.Body)
            $reason = if ($r.Status -eq 200) { 'OK' } else { 'Not Found' }
            $out = [System.Text.Encoding]::ASCII.GetBytes("HTTP/1.1 $($r.Status) $reason`r`nContent-Type: application/json`r`nContent-Length: $($payload.Length)`r`nConnection: close`r`n`r`n")
            $s.Write($out, 0, $out.Length)
            $s.Write($payload, 0, $payload.Length)
            $s.Flush()
        }
        catch { Write-Verbose "stub: $_" }
        finally { if ($client) { $client.Close() } }
    }
}

# Starts the stub on 127.0.0.1 and a free port. Returns an object with Port,
# Requests (a synchronized list the loop adds to) and what Stop-StubProvider
# needs.
function Start-StubProvider {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '', Justification = 'A test helper that only starts and stops a loopback listener.')]
    param()
    $listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    $requests = [System.Collections.ArrayList]::Synchronized((New-Object System.Collections.ArrayList))
    $stop = [pscustomobject]@{ Value = $false }
    $respond = ${function:Get-StubResponse}.ToString()
    # The runspace has no access to this script's variables, so the responder
    # and its constants travel as text.
    $consts = "`$StubModel = '$StubModel'; `$StubHeadline = '$StubHeadline'; `$StubDetail = '$StubDetail'"
    $rs = [runspacefactory]::CreateRunspace()
    $rs.Open()
    $ps = [powershell]::Create()
    $ps.Runspace = $rs
    [void]$ps.AddScript("$consts; function Get-StubResponse { $respond }; & { $($script:StubServerLoop.ToString()) } `$args[0] `$args[1] `$args[2] { param(`$m, `$p) Get-StubResponse -Method `$m -Path `$p }")
    [void]$ps.AddArgument($listener).AddArgument($requests).AddArgument($stop)
    $async = $ps.BeginInvoke()
    [pscustomobject]@{
        Port     = $port
        Requests = $requests
        Listener = $listener
        Stop     = $stop
        Shell    = $ps
        Runspace = $rs
        Async    = $async
    }
}

function Stop-StubProvider {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '', Justification = 'A test helper that only starts and stops a loopback listener.')]
    param([Parameter(Mandatory)] $Stub)
    $Stub.Stop.Value = $true
    try { $Stub.Listener.Stop() } catch { Write-Verbose "stub: $_" }
    try { [void]$Stub.Shell.EndInvoke($Stub.Async) } catch { Write-Verbose "stub: $_" }
    try { $Stub.Shell.Dispose(); $Stub.Runspace.Dispose() } catch { Write-Verbose "stub: $_" }
}
