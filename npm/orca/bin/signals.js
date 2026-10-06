// What the launcher does with each signal it takes over while the native
// binary runs, on `platform` (a `process.platform` value): "forward" passes
// the signal on to the binary, "ignore" only keeps it from ending the launcher.
//
// On Windows child.kill() is TerminateProcess whatever the signal, which would
// end orca.exe before it can stop its run and exit with 130. Ctrl+C is left to
// the console, which delivers it to orca.exe too, as it shares the launcher's
// console; the launcher stays up and exits with orca.exe's code.
export function signalDispositions(platform) {
  if (platform === "win32") {
    return { SIGINT: "ignore", SIGTERM: "forward" };
  }
  return { SIGINT: "forward", SIGTERM: "forward", SIGHUP: "forward" };
}
