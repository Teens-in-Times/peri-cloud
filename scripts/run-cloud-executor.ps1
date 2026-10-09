param([Parameter(Mandatory=$true)][string]$InstallRoot)
$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath($InstallRoot)
$taskConfig = Get-Content -LiteralPath (Join-Path $taskRoot 'executor.json') -Raw | ConvertFrom-Json
$taskBinary = Join-Path $taskRoot 'current\peri-executor.exe'
$taskState = Join-Path $taskRoot 'state'
$taskLog = Join-Path $taskRoot 'logs'
if (-not (Test-Path -LiteralPath $taskBinary)) { throw 'Installed executor is missing' }
$env:RUST_LOG = 'info'
$env:NO_PROXY = '127.0.0.1,localhost'
$taskArguments = @('--state-dir', ('"' + $taskState + '"'), '--device-name', ('"' + $taskConfig.device_name + '"'), '--listen', $taskConfig.listen, '--ssh-host', $taskConfig.ssh_host, '--ssh-reverse-listen', $taskConfig.ssh_reverse_listen, '--cloud-listen', $taskConfig.cloud_listen, '--cloud-remote', $taskConfig.cloud_remote)
while ($true) {
    # The executor owns durable tasks and the SSH child. This launcher only
    # restarts an exited executor; it never resubmits an operation.
    try {
        $taskExisting = @(Get-CimInstance Win32_Process -Filter "Name='peri-executor.exe'" | Where-Object { $_.ExecutablePath -eq $taskBinary -and $_.CommandLine.Contains($taskState) })
        if ($taskExisting.Count -gt 0) { exit 0 }
        foreach ($taskName in @('executor.out', 'executor.err')) {
            $taskPath = Join-Path $taskLog $taskName
            if (Test-Path -LiteralPath $taskPath) { Move-Item -LiteralPath $taskPath -Destination ($taskPath + '.previous') -Force }
        }
        $taskProcess = Start-Process -FilePath $taskBinary -ArgumentList $taskArguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskLog 'executor.out') -RedirectStandardError (Join-Path $taskLog 'executor.err')
        $taskProcess.WaitForExit()
        Add-Content -LiteralPath (Join-Path $taskLog 'launcher.log') -Value ("{0:o} executor exited code={1}" -f [DateTime]::UtcNow, $taskProcess.ExitCode)
    } catch {
        # Do not persist full command lines or exception data containing paths
        # from credentials. The installed log records only a failure category.
        Add-Content -LiteralPath (Join-Path $taskLog 'launcher.log') -Value ("{0:o} executor launch failed" -f [DateTime]::UtcNow)
    }
    Start-Sleep -Seconds 5
}
