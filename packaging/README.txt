trindrome
=========

trindrome looks for numbers that read the same forwards and backwards in
binary, decimal and hexadecimal at once. Your computer searches a slice of the
numbers and reports back to a shared coordinator, which hands out the slices.

It only uses your GPU while the computer is idle (no keyboard or mouse for 5
minutes) and on AC power, and it stops as soon as you come back.

Requirements
------------
- Windows 10 or 11.
- An NVIDIA RTX 30-series or newer GPU, with an up-to-date NVIDIA driver.
  Without one it falls back to the CPU, which is about a thousand times slower
  and hardly helps.

Install
-------
1. Unzip this folder anywhere.
2. Double-click install.cmd. If Windows SmartScreen says "Windows protected
   your PC", click "More info", then "Run anyway": the programs are not
   code-signed.

That's it. It installs to %LOCALAPPDATA%\Trindrome for your user only (no
administrator rights) and starts when you log in. A tray icon shows what it is
doing; right-click it to pause or change settings.

Check on it
-----------
In PowerShell:

  & "$env:LOCALAPPDATA\Trindrome\bin\trindrome.exe" status

Upgrade
-------
Unzip the new version and double-click its install.cmd. Your progress and
settings are kept.

Uninstall
---------
  & "$env:LOCALAPPDATA\Trindrome\bin\trindrome.exe" stop
  & "$env:LOCALAPPDATA\Trindrome\bin\trindrome.exe" startup disable

Then delete %LOCALAPPDATA%\Trindrome.

What it sends
-------------
The coordinator gets: a random key created on install (no account or email),
your GPU model, how fast it searches, and the results of each slice.

It also records your computer's name (e.g. DESKTOP-1234) and your network
(your IP address with the last part dropped), so that if this computer finds
something the organizer can work out whose it is. Neither is ever shown
publicly.

Your name on the public page is up to you: set it in the settings window
(right-click the tray icon), under "Shared search". Leave it empty to show a
random ID.
