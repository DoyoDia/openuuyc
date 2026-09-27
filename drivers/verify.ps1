$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$assets = Join-Path $repoRoot 'assets/drivers'
$signTool = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10/bin/10.0.26100.0/x64/signtool.exe'
if (!(Test-Path -LiteralPath $signTool)) { throw 'Windows SDK 10.0.26100.0 SignTool is required.' }

$certificate = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new((Join-Path $assets 'OpenUUYCDrivers.cer'))
try {
    if ($certificate.HasPrivateKey -or $certificate.Subject -ne 'CN=OpenUUYC Drivers') {
        throw 'The driver certificate must contain only the OpenUUYC Drivers public identity.'
    }
    $installer = Get-Content -LiteralPath (Join-Path $repoRoot 'src/platform/windows/components/mod.rs') -Raw
    $pin = [regex]::Match($installer, '"CN=OpenUUYC Drivers",\s*"(?<thumbprint>[0-9A-F]{40})"')
    if (!$pin.Success -or $pin.Groups['thumbprint'].Value -ne $certificate.Thumbprint) {
        throw 'The public certificate does not match the installer certificate pin.'
    }
    if ((Get-Date) -lt $certificate.NotBefore -or (Get-Date) -gt $certificate.NotAfter) {
        throw 'The driver certificate is outside its validity period.'
    }
    foreach ($kind in @('input', 'display')) {
        $name = if ($kind -eq 'input') { 'OpenUUYCInput' } else { 'OpenUUYCDisplay' }
        $package = Join-Path $assets $kind
        $inf = Join-Path $package "$name.inf"
        $dll = Join-Path $package "$name.dll"
        $catalog = Join-Path $package "$name.cat"
        $sourceInf = Join-Path $PSScriptRoot "$kind/$name.inf"
        if ((Get-FileHash -LiteralPath $sourceInf).Hash -ne (Get-FileHash -LiteralPath $inf).Hash) {
            throw "$kind driver source INF differs from the staged package."
        }
        foreach ($file in @($dll, $catalog)) {
            $signature = Get-AuthenticodeSignature -LiteralPath $file
            if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) {
                throw "Invalid driver signature or unexpected signer: $file ($($signature.Status)). Run this check on the configured signing machine."
            }
            if (!$signature.TimeStamperCertificate) { Write-Warning "$name has no signature timestamp: $([IO.Path]::GetFileName($file))" }
        }
        foreach ($file in @($inf, $dll)) {
            & $signTool verify /pa /c $catalog $file
            if ($LASTEXITCODE) { throw "Catalog membership/signature verification failed: $file" }
        }
        Write-Output "$name verified."
    }
    Write-Output "Driver publisher: $($certificate.Subject), $($certificate.Thumbprint)"
} finally {
    $certificate.Dispose()
}
