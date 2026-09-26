/**
 * Ultron's splash logo, shown above the startup header when the terminal is large enough. The full art is
 * 92 columns by 53 rows; a half-size version is derived from it for smaller terminals.
 */
export const ULTRON_LOGO = `                                                                                           ;
>                                                                                         :
;!                                                                                       :>
 >!                                                                                      >
  >                                                                                     >>
  >>                                                                                   >>
   >>                                                                                 >>;
   l>>                                                                               >>>
    >>>                                                                             '>>
    '>>>                                                                           i>>>
     >>>>i                                                                       ,>>>>
      >>>>>'                                                                    >>>>>i
       >>>>>>                                                                 >>>>>>I
        l>>>>>>                                                             ;>>>>>>
          >>>>>>I                                                          >>>>>>
            >>>>>>                                                       >>>>>>:
             !>>>>>>                                                   >>>>>>>
               >>>>>>I                                                >>>>>>
                 >>>>>>                                             >>>>>>:
     ,            ,>>>>>>                                         !>>>>>>
     >>,            i>>>>>!                                     .>>>>>>             >>
      >>>             >>>>>>                                   >>>>>>             >>>i
      >>>>>            .>>>>>>                               >>>>>>l            I>>>>
      i>>>>>l            !>>>>>>                           :>>>>>>             >>>>>>
       >>>>>>>             >>>>>,                          >>>>>             >>>>>>>:
       >>>>>>>>              >>>>                         !>>>I             I>>>>>>>
        >>>>>>>i              >>>                         >>>              l>>>>>>>.
        >>>>>>>>i               >                         >                >>>>>>>>
         >>>>>>>>                                                         >>>>>>>>
         >>>>>>>>>                                                       >>>>>>>>>
          >>>>>>>>>                                                     >>>>>>>>>
          I>>>>>>>>>                                                   >>>>>>>>>>
           >>>>>>>>>>.                                                >>>>>>>>>>
           .>>>>>>>>>>>>>                                         >>>>>>>>>>>>>i
            >>>>>>>>>>>>>>>>:                                 .>>>>>>>>>>>>>>>>
             >>>>>>>>>>>>>>>>>                               >>>>>>>>>>>>>>>>>;
              >>>>>>>>>>>>>>>>>>                           !>>>>>>>>>>>>>>>>>;
              :>>>>>>>>>>>>>>>>>>i                       '>>>>>>>>>>>>>>>>>>>
               >>>>>>>>>>>>>>>>>>>>      l>>>>>>>>      >>>>>>>>>>>>>>>>>>>>
                >>>>>>>>>>>>>>>>>>>>     l>>>>>>>>     I>>>>>>>>>>>>>>>>>>>
                 >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
                  >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>.
                   >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>;
                    >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>!
                    !>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
                     i>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
                      >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
                       >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
                        >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
                          >>>>>>>>>''''''!>>>>>>>>'''''';>>>>>>>>,
                           l>>>>>>       l>>>>>>>>       i>>>>>i
                             >>>>        l>>>>>>>>        !>>>
                               >                           >'`;

/** Characters ordered from least to most ink, for picking one character to stand for a 2x2 block. */
const INK = " .',:;Il!i>";

function ink(char: string): number {
	const index = INK.indexOf(char);
	return index === -1 ? INK.length : index;
}

/** Halve the art in both directions, keeping the most inked character of each 2x2 block. */
export function halveLogo(art: string): string[] {
	const rows = art.split("\n");
	const width = Math.max(...rows.map((row) => row.length));
	const grid = rows.map((row) => row.padEnd(width));
	const out: string[] = [];
	for (let y = 0; y < grid.length; y += 2) {
		let line = "";
		for (let x = 0; x < width; x += 2) {
			let best = " ";
			for (const row of [grid[y], grid[y + 1]]) {
				if (row === undefined) continue;
				for (const char of [row[x], row[x + 1]]) if (char !== undefined && ink(char) > ink(best)) best = char;
			}
			line += best;
		}
		out.push(line.trimEnd());
	}
	return out;
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
