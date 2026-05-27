Import-Module ActiveDirectory
. .\helpers.ps1

class SensorConfig {
    [string]$Name
    [double]$Threshold

    SensorConfig([string]$name, [double]$threshold) {
        $this.Name = $name
        $this.Threshold = $threshold
    }

    [string] ToString() {
        return "$($this.Name): $($this.Threshold)"
    }
}

enum Priority {
    Low
    Medium
    High
}

function Initialize-Sensor {
    param(
        [Parameter(Mandatory=$true)]
        [SensorConfig]$Config
    )

    Write-Host "Initializing sensor: $($Config.Name)"
    Set-Threshold -Value $Config.Threshold
}

function Get-SensorData {
    param(
        [string]$SensorName
    )

    $data = Get-WmiObject -Class Win32_Sensor
    return $data | Where-Object { $_.Name -eq $SensorName }
}

filter Select-ActiveSensors {
    if ($_.Status -eq "Active") {
        $_
    }
}

function Main {
    $config = [SensorConfig]::new("temp-1", 100.0)
    Initialize-Sensor -Config $config
    $data = Get-SensorData -SensorName "temp-1"
    Write-Output $data
}

Main
