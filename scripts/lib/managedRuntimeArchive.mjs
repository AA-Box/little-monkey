import { extname } from "node:path";

/**
 * Chooses the host archive tool without interpolating archive paths into a
 * shell command. GNU tar cannot extract ZIP assets, while Windows' bsdtar can.
 */
export function managedRuntimeArchiveExtractor(archivePath, extractRoot, platform = process.platform) {
  if (extname(archivePath).toLowerCase() === ".zip" && platform !== "win32") {
    return ["unzip", ["-q", archivePath, "-d", extractRoot]];
  }
  return ["tar", ["-xf", archivePath, "-C", extractRoot]];
}
