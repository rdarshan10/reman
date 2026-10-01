# reman's question window (Windows): may an agent see a folder's command history? Prints the
# choice. Run by `reman mcp` (mcp.rs, window::ask) with -STA, only when the user turned it on
# ("consent_window": true). WPF: crisp at any display scaling, and it follows light / dark mode.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName PresentationFramework, PresentationCore, WindowsBase

$app = '@@APP@@'
$folder = '@@FOLDER@@'
$session = '@@SESSION@@'
$always = '@@ALWAYS@@'
$no = '@@NO@@'
# (a path here: draw the window into a PNG instead of showing it - to check how it looks)
$render = '@@RENDER@@'

$dark = $false
try {
  $dark = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize' -Name AppsUseLightTheme).AppsUseLightTheme -eq 0
} catch { }
$c = if ($dark) {
  @{ Bg = '#202124'; Fg = '#F3F3F5'; Muted = '#A3A3AD'; Line = '#36363D'; Box = '#2A2A30'; Btn = '#303037'; BtnHover = '#3B3B43'; Accent = '#3B82F6'; AccentHover = '#2F6FE0'; Shadow = '0.55' }
} else {
  @{ Bg = '#FFFFFF'; Fg = '#18181B'; Muted = '#62626B'; Line = '#E3E3E8'; Box = '#F4F4F7'; Btn = '#EFEFF3'; BtnHover = '#E4E4EA'; Accent = '#2563EB'; AccentHover = '#1D4ED8'; Shadow = '0.22' }
}

$xaml = @"
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="reman" Width="492" SizeToContent="Height" WindowStyle="None" AllowsTransparency="True"
        Background="Transparent" ResizeMode="NoResize" Topmost="True" ShowInTaskbar="True"
        WindowStartupLocation="CenterScreen" FontFamily="Segoe UI" UseLayoutRounding="True"
        TextOptions.TextFormattingMode="Display">
  <Window.Resources>
    <Style x:Key="Btn" TargetType="Button">
      <Setter Property="Foreground" Value="$($c.Fg)"/>
      <Setter Property="Background" Value="$($c.Btn)"/>
      <Setter Property="BorderBrush" Value="Transparent"/>
      <Setter Property="FontSize" Value="13"/>
      <Setter Property="Padding" Value="14,7,14,8"/>
      <Setter Property="Margin" Value="8,0,0,0"/>
      <Setter Property="Cursor" Value="Hand"/>
      <Setter Property="FocusVisualStyle" Value="{x:Null}"/>
      <Setter Property="Template">
        <Setter.Value>
          <ControlTemplate TargetType="Button">
            <Border Background="{TemplateBinding Background}" BorderBrush="{TemplateBinding BorderBrush}" BorderThickness="1.5"
                    CornerRadius="6" Padding="{TemplateBinding Padding}">
              <ContentPresenter HorizontalAlignment="Center" VerticalAlignment="Center"/>
            </Border>
          </ControlTemplate>
        </Setter.Value>
      </Setter>
      <Style.Triggers>
        <Trigger Property="IsMouseOver" Value="True"><Setter Property="Background" Value="$($c.BtnHover)"/></Trigger>
        <Trigger Property="IsKeyboardFocused" Value="True"><Setter Property="BorderBrush" Value="$($c.Accent)"/></Trigger>
      </Style.Triggers>
    </Style>
    <Style x:Key="Primary" TargetType="Button" BasedOn="{StaticResource Btn}">
      <Setter Property="Foreground" Value="White"/>
      <Setter Property="Background" Value="$($c.Accent)"/>
      <Setter Property="FontWeight" Value="SemiBold"/>
      <Style.Triggers>
        <Trigger Property="IsMouseOver" Value="True"><Setter Property="Background" Value="$($c.AccentHover)"/></Trigger>
        <Trigger Property="IsKeyboardFocused" Value="True"><Setter Property="BorderBrush" Value="$($c.Fg)"/></Trigger>
      </Style.Triggers>
    </Style>
  </Window.Resources>
  <Border Margin="18" Background="$($c.Bg)" CornerRadius="10" BorderBrush="$($c.Line)" BorderThickness="1">
    <Border.Effect><DropShadowEffect BlurRadius="22" ShadowDepth="3" Direction="270" Opacity="$($c.Shadow)"/></Border.Effect>
    <StackPanel Margin="24,18,24,22">
      <StackPanel Orientation="Horizontal" Margin="0,0,0,14">
        <Border Width="22" Height="22" CornerRadius="6" Background="$($c.Accent)">
          <TextBlock Text="&#x203A;_" Foreground="White" FontFamily="Cascadia Mono, Consolas" FontWeight="Bold" FontSize="11"
                     HorizontalAlignment="Center" VerticalAlignment="Center"/>
        </Border>
        <TextBlock Text="reman" Foreground="$($c.Muted)" FontSize="12.5" Margin="8,0,0,1" VerticalAlignment="Center"/>
      </StackPanel>
      <TextBlock Text="Share your command history?" Foreground="$($c.Fg)" FontSize="19" FontWeight="SemiBold"/>
      <TextBlock x:Name="Ask" Foreground="$($c.Fg)" FontSize="13.5" TextWrapping="Wrap" Margin="0,8,0,12" LineHeight="20"/>
      <Border Background="$($c.Box)" CornerRadius="6" Padding="11,8">
        <TextBlock x:Name="Folder" Foreground="$($c.Fg)" FontFamily="Cascadia Mono, Consolas" FontSize="12.5" TextTrimming="CharacterEllipsis"/>
      </Border>
      <TextBlock Foreground="$($c.Muted)" FontSize="12" TextWrapping="Wrap" Margin="0,12,0,20" LineHeight="18"
                 Text="Secrets stay masked, and nothing leaves this computer. Allowing it for this session lasts until this chat ends; you'll be asked again next time."/>
      <StackPanel Orientation="Horizontal" HorizontalAlignment="Right">
        <Button x:Name="No" Style="{StaticResource Btn}" IsCancel="True" Content="Don't allow"/>
        <Button x:Name="Always" Style="{StaticResource Btn}" Content="Always allow"/>
        <Button x:Name="Session" Style="{StaticResource Primary}" IsDefault="True" Content="Allow for this session"/>
      </StackPanel>
    </StackPanel>
  </Border>
</Window>
"@

$w = [Windows.Markup.XamlReader]::Parse($xaml)
$w.FindName('Ask').Text = "$app wants to see the commands you and your agents ran in this folder:"
$f = $w.FindName('Folder')
$f.Text = $folder
$f.ToolTip = $folder
$script:choice = $null
$w.FindName('Session').Add_Click({ $script:choice = $session; $w.Close() })
$w.FindName('Always').Add_Click({ $script:choice = $always; $w.Close() })
$w.FindName('No').Add_Click({ $script:choice = $no; $w.Close() })
$w.Add_MouseLeftButtonDown({ try { $w.DragMove() } catch { } })
$w.Add_ContentRendered({ $w.Activate() | Out-Null; [void]$w.FindName('Session').Focus() })

if ($render) {
  # lay it out without showing it, and draw it at 2x
  $card = $w.Content
  $w.Content = $null
  $card.Measure([Windows.Size]::new($w.Width, [double]::PositiveInfinity))
  $card.Arrange([Windows.Rect]::new($card.DesiredSize))
  # (DesiredSize includes the margin the shadow is drawn in)
  $bmp = [Windows.Media.Imaging.RenderTargetBitmap]::new([int]($card.DesiredSize.Width * 2), [int]($card.DesiredSize.Height * 2), 192, 192, [Windows.Media.PixelFormats]::Pbgra32)
  $bmp.Render($card)
  $png = [Windows.Media.Imaging.PngBitmapEncoder]::new()
  $png.Frames.Add([Windows.Media.Imaging.BitmapFrame]::Create($bmp))
  $out = [IO.File]::Create($render)
  $png.Save($out)
  $out.Close()
  exit
}

[void]$w.ShowDialog()
if ($script:choice) { [Console]::Out.Write($script:choice) }
