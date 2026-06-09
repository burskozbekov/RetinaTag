$ErrorActionPreference = 'Continue'
Write-Host "=== Store image extensions installed? ==="
$heif = Get-AppxPackage -Name "Microsoft.HEIFImageExtension" -ErrorAction SilentlyContinue
$raw  = Get-AppxPackage -Name "Microsoft.RawImageExtension"  -ErrorAction SilentlyContinue
Write-Host ("HEIF Image Extension: {0}" -f $(if($heif){$heif.Version}else{'NOT INSTALLED'}))
Write-Host ("Raw  Image Extension: {0}" -f $(if($raw){$raw.Version}else{'NOT INSTALLED'}))

function Test-WpfDecode($path,$label){
  if(-not (Test-Path $path)){ Write-Host ("{0}: FILE MISSING {1}" -f $label,$path); return }
  try{
    Add-Type -AssemblyName PresentationCore
    $uri = New-Object System.Uri(('file:///' + ($path -replace '\\','/')))
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $dec = [System.Windows.Media.Imaging.BitmapDecoder]::Create($uri, [System.Windows.Media.Imaging.BitmapCreateOptions]::PreservePixelFormat, [System.Windows.Media.Imaging.BitmapCacheOption]::OnLoad)
    $f = $dec.Frames[0]
    $sw.Stop()
    Write-Host ("{0}: OK  {1}x{2}  {3}ms  {4}" -f $label,$f.PixelWidth,$f.PixelHeight,$sw.ElapsedMilliseconds,(Split-Path $path -Leaf))
  }catch{
    Write-Host ("{0}: FAIL {1}  ::  {2}" -f $label,(Split-Path $path -Leaf),$_.Exception.Message)
  }
}
Write-Host "`n=== WPF decode tests (current scanner path) ==="
Test-WpfDecode 'D:\Fotograflar\2020\01-January\IMG_4429.HEIC' 'HEIC'
Test-WpfDecode 'D:\Fotograflar\2020\01-January\IMG_4434.HEIC' 'HEIC'
Test-WpfDecode 'D:\Fotograflar\2007\09-September\IMG_1809.CR2' 'RAW '
Test-WpfDecode 'D:\Fotograflar\2007\10-October\IMG_1943.CR2' 'RAW '
