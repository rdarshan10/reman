// reman's question window (macOS): may an agent see a folder's command history? A native alert
// (NSAlert, like the system's own), on top, following light / dark mode. Prints the choice.
// Run by `reman mcp` (mcp.rs, window::ask) as:
//   osascript -l JavaScript consent-macos.js <app> <folder> <session> <always> <no>
ObjC.import('AppKit');

function run(argv) {
  const [app, folder, session, always, no] = argv;
  const ns = $.NSApplication.sharedApplication;
  // an accessory: no Dock icon, but allowed in front of other apps
  ns.setActivationPolicy($.NSApplicationActivationPolicyAccessory);

  const alert = $.NSAlert.alloc.init;
  alert.messageText = 'Share your command history?';
  alert.informativeText = app + ' wants to see the commands you and your agents ran in this folder:\n\n' + folder +
    '\n\nSecrets stay masked, and nothing leaves this computer. Allowing it for this session lasts until this chat ends; ' +
    "you'll be asked again next time.";
  alert.addButtonWithTitle(session);   // the default: Return
  alert.addButtonWithTitle(always);
  const dont = alert.addButtonWithTitle(no);
  dont.keyEquivalent = '\u001b';        // Esc

  alert.window.level = $.NSFloatingWindowLevel;
  ns.activateIgnoringOtherApps(true);
  const r = alert.runModal;
  if (r == $.NSAlertFirstButtonReturn) return session;
  if (r == $.NSAlertSecondButtonReturn) return always;
  return no;
}
