// The site's icons come from the app icon in the repository (assets/icons/maze.svg),
// cropped to the icon body: the app icon has a 100 px transparent margin for the
// macOS icon grid, which would make a favicon look tiny.

import appIcon from "../../../assets/icons/maze.svg?raw";

export function croppedIconSvg(): string {
  return appIcon
    .replace(/<!--[\s\S]*?-->\s*/g, "")
    .replace(
      'width="1024" height="1024" viewBox="0 0 1024 1024"',
      'width="824" height="824" viewBox="100 100 824 824"',
    )
    .replace(' filter="url(#shadow)"', "");
}
