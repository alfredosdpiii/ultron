/**
 * Ultron's splash logo, shown above the startup header when the terminal is large enough. The art is a negative:
 * \`$\` is the background and every other character is part of the logo, so it renders two-tone (background dim,
 * logo in the accent colour). The full art is 100 columns by 55 rows; a half-size version is derived from it
 * for smaller terminals.
 */
export const LOGO_BACKGROUND = "$";
export const ULTRON_LOGO = `$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$a$$$$
$$$$d$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$o$$$$$
$$$$ab$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$od$$$$$
$$$$$db$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$Bd$$$$$$
$$$$$WdB$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dd$$$$$$
$$$$$$dd%$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dd$$$$$$$
$$$$$$@dd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dda$$$$$$$
$$$$$$$kdp$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddd$$$$$$$$
$$$$$$$$ddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$#ddW$$$$$$$$
$$$$$$$$#ddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dpdd$$$$$$$$$
$$$$$$$$$ddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$*ddddB$$$$$$$$$
$$$$$$$$$%ddddd#$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dddddd$$$$$$$$$$
$$$$$$$$$$&dddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddh$$$$$$$$$$$
$$$$$$$$$$$$kdddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$adddddp$$$$$$$$$$$$$
$$$$$$$$$$$$$$dddpddh$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$%ddddddB$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$8dddddd%$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dddppdo$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$bdddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddd$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$ddddddh$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$WddddddB$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$ddddddB$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dddddd*$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$*$$$$$$$$$$$$*dddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$bdddddd$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$dd*$$$$$$$$$$$$ddddddb$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$Mddddpd$$$$$$$$$$$$%dd$$$$$$$$$$
$$$$$$$$$8ddd$$$$$$$$$$$$$ddddddW$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dddddd&$$$$$$$$$$$$dddd$$$$$$$$$$
$$$$$$$$$$pdddd$$$$$$$$$$$$Mpddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddk$$$$$$$$$$$$hdddd$$$$$$$$$$$
$$$$$$$$$$dddpddh$$$$$$$$$$$$bpddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$odpdddd$$$$$$$$$$$$%dddddd$$$$$$$$$$$
$$$$$$$$$$$ddddddd$$$$$$$$$$$$$ddddd*$$$$$$$$$$$$$$$$$$$$$$$$$$ddddd%$$$$$$$$$$$$dddddddo$$$$$$$$$$$
$$$$$$$$$$$dddddddd$$$$$$$$$$$$$Wdddd$$$$$$$$$$$$$$$$$$$$$$$$$kdddh$$$$$$$$$$$$$hddddddd$$$$$$$$$$$$
$$$$$$$$$$$$dddddddd$$$$$$$$$$$$$$ddd$$$$$$$$$$$$$$$$$$$$$$$$$ddd$$$$$$$$$$$$$$kdddddddW$$$$$$$$$$$$
$$$$$$$$$$$$ddddpdddd$$$$$$$$$$$$$$$d$$$$$$$$$$$$$$$$$$$$$$$$$d%$$$$$$$$$$$$$$%ddddpddd$$$$$$$$$$$$$
$$$$$$$$$$$$$pddddddd8$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dddddddd@$$$$$$$$$$$$$
$$$$$$$$$$$$$ddddddddd&$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddddd$$$$$$$$$$$$$$
$$$$$$$$$$$$$$dddpddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddddd$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$hddddddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddpddd$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$ddddppddddM$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$8dddddddddd$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$Mddddddddddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dddddddddddddd$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$pdddddddddddddddo$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$Mdddddddddddpdddp$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$Bddddddddddddddddp%$$$$$$$$$$$$$$$$$$$$$$$$$$$$$@ddddddddddddddddda$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$Bdddddddddddddddpdd$$$$$$$$$$$$$$$$$$$$$$$$$$$bddddddddddddddddda$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$oddddddddddddddddpdb$$$$$$$&WWWWWWWW$$$$$$$Mddddddddddddddddddd$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$ddddpddddddddddddddd$$$$$$hdddddddd$$$$$$dddddddddddddpdddddd$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$dddddddddddddddddddd$$$$$hdddddddd$$$$$addpdddpdddddddddddd$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$dddddddddddddddddddddddddddddddddpddddddddddddddddddddddd$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$dddddddddddddddddddddddddddddddddddddpddpddddddddddddddM$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$8dddddpdddddddddpdddddddddddddddppddddddddddpddddddddda$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$%dddddddddddddddddddddddddddddddddddddpddddpdddddddpb$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$bddddddpdddddddddddddddddddddddddddddddddddpddddddd$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$ddddddddpdddddddddddddddddddddddddddddddddddddddd$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$dddddddddddddddddddddddddddddddpdddddddpddddddd@$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddddddddddddddddddddddddddpdddddddddddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$$ddddddddddddddddddddddddddpdddddddddddddddd@$$$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$$$%dpddddddd######bdddddddd######adddddddd*$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$hdddddd$$$$$$$hdddddddd$$$$$$$bdddddd$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$dddd$$$$$$$$hdppddddd$$$$$$$$bddd@$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$d$$$$$$$$$$$$$$$$$$$$$$$$$$$d#$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$
$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$`;

/** Background (and padding) carries no ink; any logo character does, denser letters slightly more. */
const INK = "$ .',:;Il!i>";

function ink(char: string): number {
	if (char === LOGO_BACKGROUND || char === " ") return 0;
	const index = INK.indexOf(char);
	return index === -1 ? INK.length : index;
}

/** Halve the art in both directions, keeping the most inked character of each 2x2 block. */
export function halveLogo(art: string): string[] {
	const rows = art.split("\n");
	const width = Math.max(...rows.map((row) => row.length));
	const grid = rows.map((row) => row.padEnd(width, LOGO_BACKGROUND));
	const out: string[] = [];
	for (let y = 0; y < grid.length; y += 2) {
		let line = "";
		for (let x = 0; x < width; x += 2) {
			let best = LOGO_BACKGROUND;
			for (const row of [grid[y], grid[y + 1]]) {
				if (row === undefined) continue;
				for (const char of [row[x], row[x + 1]]) if (char !== undefined && ink(char) > ink(best)) best = char;
			}
			line += best;
		}
		out.push(line);
	}
	return out;
}

/**
 * The half-size logo as plain text, for use outside the terminal (a comment, a document): the background is
 * blank, lines carry no trailing spaces, and empty rows above and below and the common left margin are removed.
 */
export function logoText(): string {
	const rows = halveLogo(ULTRON_LOGO).map((row) => row.replaceAll(LOGO_BACKGROUND, " ").trimEnd());
	while (rows.length > 0 && rows[0] === "") rows.shift();
	while (rows.length > 0 && rows.at(-1) === "") rows.pop();
	const inked = rows.filter((row) => row !== "");
	const indent = inked.length === 0 ? 0 : Math.min(...inked.map((row) => row.length - row.trimStart().length));
	return rows.map((row) => row.slice(indent)).join("\n");
}

const FULL = ULTRON_LOGO.split("\n");
const HALF = halveLogo(ULTRON_LOGO);
/** Terminal rows the rest of the UI needs below the splash (header, editor, footer). */
const RESERVED_ROWS = 14;

function fits(logo: readonly string[], width: number, rows: number | undefined): boolean {
	const logoWidth = Math.max(...logo.map((line) => line.length));
	return logoWidth <= width && (rows === undefined || logo.length + RESERVED_ROWS <= rows);
}

/**
 * The splash lines for a terminal of this size, centered, or none when even the half-size logo does not fit.
 * `rows` is the terminal height when known.
 */
export function splashLines(width: number, rows: number | undefined): string[] {
	const logo = fits(FULL, width, rows) ? FULL : fits(HALF, width, rows) ? HALF : undefined;
	if (!logo) return [];
	const logoWidth = Math.max(...logo.map((line) => line.length));
	const pad = " ".repeat(Math.max(0, Math.floor((width - logoWidth) / 2)));
	return logo.map((line) => (line.length === 0 ? "" : pad + line));
}

/** Colour a splash line: runs of background in one style and logo characters in another; padding stays plain. */
export function colorLogoLine(
	line: string,
	logo: (text: string) => string,
	background: (text: string) => string,
): string {
	return line.replace(/( +)|(\$+)|([^ $]+)/g, (_match, pad: string, bg: string, fg: string) =>
		pad !== undefined ? pad : bg !== undefined ? background(bg) : logo(fg),
	);
}
