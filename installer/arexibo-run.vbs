' Starts the player and starts it again when it exits, as systemd does on
' Linux (Restart=always, RestartSec=3): after the registration with a code,
' a change of CMS and so on, the player exits and expects to be started again.
' The arguments, such as the CMS address and key typed in the installer, are
' for the first start only: after it the player has its own settings, and
' they must not be given again (a change of CMS would be undone).

Set shell = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
exe = fso.BuildPath(fso.GetParentFolderName(WScript.ScriptFullName), "arexibo.exe")

args = ""
For Each arg In WScript.Arguments
  args = args & " """ & arg & """"
Next

Do
  shell.Run """" & exe & """" & args, 1, True
  args = ""
  WScript.Sleep 3000
Loop
