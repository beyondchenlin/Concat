# SPDX-License-Identifier: AGPL-3.0-or-later
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$JdkDirectory,
    [Parameter(Mandatory=$true)][string]$AndroidSdkDirectory,
    [Parameter(Mandatory=$true)][string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
$bkCrate = Split-Path -Parent $PSScriptRoot
$bkJar = Join-Path $AndroidSdkDirectory 'platforms\android-35\android.jar'
$bkJavac = Join-Path $JdkDirectory 'bin\javac.exe'
$bkJava = Join-Path $JdkDirectory 'bin\java.exe'
$bkDexer = Join-Path $AndroidSdkDirectory 'build-tools\35.0.1\d8.bat'
foreach ($bkRequired in @($bkJar,$bkJavac,$bkJava,$bkDexer)) {
    if (-not (Test-Path -LiteralPath $bkRequired)) { throw "Required tool missing: $bkRequired" }
}
if ((Test-Path -LiteralPath $OutputDirectory) -and
    @(Get-ChildItem -LiteralPath $OutputDirectory -Force).Count -ne 0) {
    throw 'Choose an empty output directory; stale classes cannot be packaging evidence.'
}
$bkClasses = Join-Path $OutputDirectory 'classes'
$bkTests = Join-Path $OutputDirectory 'tests'
$bkDex = Join-Path $OutputDirectory 'dex'
New-Item -ItemType Directory -Path $bkClasses,$bkTests,$bkDex -Force | Out-Null
$bkPreviousJava = $env:JAVA_HOME
try {
    $env:JAVA_HOME = $JdkDirectory
    $bkSources = @('ConcatFiles.java','BKCatRuntime.java','BKCatRuntimePolicy.java','BKCatCredentials.java','BKCatCredentialPolicy.java') |
        ForEach-Object { Join-Path $bkCrate ('java\' + $_) }
    $ErrorActionPreference = 'Continue'
    & $bkJavac -encoding UTF-8 -source 8 -target 8 -Xlint:-options -classpath $bkJar -d $bkClasses @bkSources > (Join-Path $OutputDirectory 'javac.log') 2>&1
    $bkCode = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    Get-Content -LiteralPath (Join-Path $OutputDirectory 'javac.log')
    if ($bkCode -ne 0) { throw "Android Java compile failed: $bkCode" }
    Write-Output 'Android API 35 Java compilation passed.'
    & $bkJavac -encoding UTF-8 -source 8 -target 8 -Xlint:-options -classpath $bkClasses -d $bkTests (Join-Path $bkCrate 'tests\BKCatRuntimePolicyTest.java')
    if ($LASTEXITCODE -ne 0) { throw 'Runtime policy test compilation failed.' }
    & $bkJava -cp ($bkClasses + ';' + $bkTests) app.concat.editor.BKCatRuntimePolicyTest |
        Tee-Object -FilePath (Join-Path $OutputDirectory 'policy-tests.log')
    if ($LASTEXITCODE -ne 0) { throw 'Runtime policy tests failed.' }
    & $bkJavac -encoding UTF-8 -source 8 -target 8 -Xlint:-options -classpath $bkClasses -d $bkTests (Join-Path $bkCrate 'tests\BKCatCredentialPolicyTest.java')
    if ($LASTEXITCODE -ne 0) { throw 'Credential envelope test compilation failed.' }
    & $bkJava -cp ($bkClasses + ';' + $bkTests) app.concat.editor.BKCatCredentialPolicyTest |
        Tee-Object -FilePath (Join-Path $OutputDirectory 'credential-policy-tests.log')
    if ($LASTEXITCODE -ne 0) { throw 'Credential envelope tests failed.' }
    $bkCompiled = @(Get-ChildItem -LiteralPath $bkClasses -Recurse -Filter '*.class' | ForEach-Object { $_.FullName })
    $ErrorActionPreference = 'Continue'
    & $bkDexer --lib $bkJar --min-api 26 --release --output $bkDex @bkCompiled > (Join-Path $OutputDirectory 'd8.log') 2>&1
    $bkCode = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    Get-Content -LiteralPath (Join-Path $OutputDirectory 'd8.log')
    if ($bkCode -ne 0) { throw "Android DEX compilation failed: $bkCode" }
    Get-FileHash -LiteralPath (Join-Path $bkDex 'classes.dex') -Algorithm SHA256 |
        Format-List | Out-String | Tee-Object -FilePath (Join-Path $OutputDirectory 'dex-sha256.txt')
    Write-Output 'DEX packaging passed. Credential envelope tests used a JDK test key, not AndroidKeyStore. Android native link, APK installation and device execution were NOT run by this script.'
} finally {
    $env:JAVA_HOME = $bkPreviousJava
}
