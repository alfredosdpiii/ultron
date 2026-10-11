//! Differential parity of the LaTeX renderer against packages/tui/src/latex.ts (TypeScript).
//! Fixture: fixtures/latex-parity.json, produced by `node crates/tui/fixtures/generate-latex.mjs`.

use serde_json::Value;
use tui::latex::render_latex;

fn fixture() -> Vec<Value> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/latex-parity.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
    v["cases"].as_array().expect("cases").clone()
}

fn check(display: bool, key: &str) {
    let cases = fixture();
    let mut failures = Vec::new();
    for c in &cases {
        let input = c["input"].as_str().expect("input");
        let want = c[key].as_str().map(str::to_string);
        let got = render_latex(input, display);
        if got != want {
            failures.push(format!("{input:?}\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} differ:\n{}",
        failures.len(),
        cases.len(),
        failures[..failures.len().min(15)].join("\n")
    );
}

#[test]
fn tui_latex_inline_matches_ts() {
    check(false, "inline");
}

#[test]
fn tui_latex_display_matches_ts() {
    check(true, "display");
}

/// The `defineCases` tables of packages/tui/test/latex.test.ts (inline rendering), with their literal expectations.
#[test]
fn tui_latex_ported_ts_cases() {
    let cases: &[(&str, &str)] = &[
        (r###"\mathbb{C}^3 \to \mathbb{C}^3"###, r###"ℂ³ → ℂ³"###),
        (
            r###"\{3x+2y,\; 27x^2-4z-1,\; x(x-1)(x+1)\} \quad\Rightarrow\quad x \in \{0, \pm 1\},"###,
            r###"{3x+2y, 27x²-4z-1, x(x-1)(x+1)} ⇒ x ∈ {0, ± 1},"###,
        ),
        (r###"F_1 = -\frac{1}{4x^2}."###, r###"F₁ = -1/(4x²)."###),
        (r###"-2"###, r###"-2"###),
        (r###"(0,0,-1/4)"###, r###"(0,0,-1/4)"###),
        (r###"(1,-3/2,13/2)"###, r###"(1,-3/2,13/2)"###),
        (r###"(1,1,1)"###, r###"(1,1,1)"###),
        (r###"(2,1,0)"###, r###"(2,1,0)"###),
        (r###"(-1/4, 0, 0)"###, r###"(-1/4, 0, 0)"###),
        (
            r###"\{(0,0,-1/4), (1,-3/2,13/2), (-1,3/2,13/2)\}"###,
            r###"{(0,0,-1/4), (1,-3/2,13/2), (-1,3/2,13/2)}"###,
        ),
        (r###"(2,1,1)"###, r###"(2,1,1)"###),
        (r###"(7/3,-2/5,11/7)"###, r###"(7/3,-2/5,11/7)"###),
        (r###"\{y - p(x),\; q(x)\}"###, r###"{y - p(x), q(x)}"###),
        (r###"\deg q = 3"###, r###"deg q = 3"###),
        (
            r###"[\mathbb{C}(x,y,z):\mathbb{C}(F_1,F_2,F_3)] = 3"###,
            r###"[ℂ(x,y,z):ℂ(F₁,F₂,F₃)] = 3"###,
        ),
        (r###"u = 1+xy"###, r###"u = 1+xy"###),
        (r###"G = u^2 z + y^2(4+3xy)"###, r###"G = u² z + y²(4+3xy)"###),
        (r###"F_1 = uG"###, r###"F₁ = uG"###),
        (r###"F_2 = y + 3xG"###, r###"F₂ = y + 3xG"###),
        (r###"x=0"###, r###"x = 0"###),
        (r###"F_2 = F_3 = 0"###, r###"F₂ = F₃ = 0"###),
        (r###"xy = -3/2"###, r###"xy = -3/2"###),
        (r###"x^2 z = 13/2"###, r###"x² z = 13/2"###),
        (r###"\mathbb{C}^*"###, r###"ℂ^*"###),
        (
            r###"s \mapsto (s,\, -\tfrac{3}{2s},\, \tfrac{13}{2s^2})"###,
            r###"s ↦ (s, -3/(2s), 13/(2s²))"###,
        ),
        (r###"X"###, r###"X"###),
        (r###"p_\pm"###, r###"p_±"###),
        (
            r###"F(-x,-y,z) = (F_1, -F_2, -F_3)"###,
            r###"F(-x,-y,z) = (F₁, -F₂, -F₃)"###,
        ),
        (r###"p_0"###, r###"p₀"###),
        (r###"s \to \infty"###, r###"s → ∞"###),
        (r###"(0,0,0)"###, r###"(0,0,0)"###),
        (r###"\Rightarrow"###, r###"⇒"###),
        (r###"\ge 2"###, r###"≥ 2"###),
        (r###"\ge 3"###, r###"≥ 3"###),
        (r###"1"###, r###"1"###),
        (
            r###"\mathrm{diag}(-1/2,1,1),\quad F_{\rm intrinsic}(\lambda)"###,
            r###"diag(-1/2,1,1), F_intrinsic(λ)"###,
        ),
        (r###"4+3xy"###, r###"4+3xy"###),
        (
            r###"E \approx \frac{0.1\ \text{lux}}{100\ \text{lm/W}} = 0.001\ \text{W/m}^2"###,
            r###"E ≈ (0.1 lux)/(100 lm/W) = 0.001 W/m²"###,
        ),
        (
            r###"\boxed{1\ \text{milliwatt per square metre}}"###,
            r###"[1 milliwatt per square metre]"###,
        ),
        (
            r###"5\ \text{km}^2 = 5{,}000{,}000\ \text{m}^2"###,
            r###"5 km² = 5,000,000 m²"###,
        ),
        (
            "P_{\\text{light}} = 0.001 \\times 5{,}000{,}000\n= \\boxed{5{,}000\\ \\text{W}}",
            r###"P_light = 0.001 × 5,000,000 = [5,000 W]"###,
        ),
        (
            "P_{\\text{electric}} = 5\\ \\text{kW} \\times 0.2\n= \\boxed{1\\ \\text{kW}}",
            r###"P_electric = 5 kW × 0.2 = [1 kW]"###,
        ),
        (
            r###"\pi(2.5\ \text{km})^2 = 19.6\ \text{km}^2"###,
            r###"π(2.5 km)² = 19.6 km²"###,
        ),
        (
            "0.001\\ \\text{W/m}^2 \\times 19.6 \\times 10^6\\ \\text{m}^2\n\\approx \\boxed{20\\ \\text{kW optical}}",
            r###"0.001 W/m² × 19.6 × 10⁶ m² ≈ [20 kW optical]"###,
        ),
        (
            "1\\ \\text{kW} \\times \\frac{1}{3600}\\ \\text{hour}\n= \\boxed{0.28\\ \\text{Wh}}",
            r###"1 kW × 1/3600 hour = [0.28 Wh]"###,
        ),
        (
            r###"\det\!\left(\frac{\partial(F_1,F_2,F_3)}{\partial(x,y,z)}\right)=-2."###,
            r###"det((∂(F₁,F₂,F₃))/(∂(x,y,z))) = -2."###,
        ),
        (
            "\\begin{aligned}\nF(0,0,-\\tfrac14)&=(-\\tfrac14,0,0),\\\\\nF(1,-\\tfrac32,\\tfrac{13}2)&=(-\\tfrac14,0,0),\\\\\nF(-1,\\tfrac32,\\tfrac{13}2)&=(-\\tfrac14,0,0).\n\\end{aligned}",
            "F(0,0,-1/4) = (-1/4,0,0),\nF(1,-3/2,13/2) = (-1/4,0,0),\nF(-1,3/2,13/2) = (-1/4,0,0).",
        ),
        (r###"F=(F_1,F_2,F_3)"###, r###"F = (F₁,F₂,F₃)"###),
        (r###"F"###, r###"F"###),
        (r###"3"###, r###"3"###),
        (
            "J = \\begin{pmatrix}\n\\frac{\\partial f_1}{\\partial x} & \\frac{\\partial f_1}{\\partial y} & \\frac{\\partial f_1}{\\partial z} \\\\\n\\frac{\\partial f_2}{\\partial x} & \\frac{\\partial f_2}{\\partial y} & \\frac{\\partial f_2}{\\partial z} \\\\\n\\frac{\\partial f_3}{\\partial x} & \\frac{\\partial f_3}{\\partial y} & \\frac{\\partial f_3}{\\partial z}\n\\end{pmatrix}",
            "J = ⎛ (∂ f₁)/(∂ x) │ (∂ f₁)/(∂ y) │ (∂ f₁)/(∂ z) ⎞\n    ⎜ (∂ f₂)/(∂ x) │ (∂ f₂)/(∂ y) │ (∂ f₂)/(∂ z) ⎟\n    ⎝ (∂ f₃)/(∂ x) │ (∂ f₃)/(∂ y) │ (∂ f₃)/(∂ z) ⎠",
        ),
        (
            "\\begin{aligned}\nf_1 &= (1+xy)^3 z + y^2(1+xy)(4+3xy) \\\\\nf_2 &= y + 3x(1+xy)^2 z + 3xy^2(4+3xy) \\\\\nf_3 &= 2x - 3x^2y - x^3z\n\\end{aligned}",
            "f₁ = (1+xy)³ z + y²(1+xy)(4+3xy)\nf₂ = y + 3x(1+xy)² z + 3xy²(4+3xy)\nf₃ = 2x - 3x²y - x³z",
        ),
        (r###"x, y, z"###, r###"x, y, z"###),
        (r###"(x, y, z)"###, r###"(x, y, z)"###),
        (r###"(0,\; 0,\; -\tfrac14)"###, r###"(0, 0, -1/4)"###),
        (r###"(-\tfrac14,\; 0,\; 0)"###, r###"(-1/4, 0, 0)"###),
        (r###"(1,\; -\tfrac32,\; \tfrac{13}{2})"###, r###"(1, -3/2, 13/2)"###),
        (r###"(-1,\; \tfrac32,\; \tfrac{13}{2})"###, r###"(-1, 3/2, 13/2)"###),
        (r###"(-\frac14, 0, 0)"###, r###"(-1/4, 0, 0)"###),
        (r###"F: \mathbb{C}^3 \to \mathbb{C}^3"###, r###"F: ℂ³ → ℂ³"###),
        (
            r###"F(0,0,-\tfrac14) = F(1,-\tfrac32,\tfrac{13}{2}) = F(-1,\tfrac32,\tfrac{13}{2}) = (-\tfrac14, 0, 0)"###,
            r###"F(0,0,-1/4) = F(1,-3/2,13/2) = F(-1,3/2,13/2) = (-1/4, 0, 0)"###,
        ),
        (r###"\mathbb{C}^3"###, r###"ℂ³"###),
        (
            "\\begin{aligned}\nf_1 &= \\frac{f_1^{\\text{ut}}(u,t)}{x^2}, \\quad\nf_2 = \\frac{f_2^{\\text{ut}}(u,t)}{x}, \\quad\nf_3 = x\\,(2 - 3u - t)\n\\end{aligned}",
            r###"f₁ = (f₁ᵘᵗ(u,t))/(x²), f₂ = (f₂ᵘᵗ(u,t))/x, f₃ = x (2 - 3u - t)"###,
        ),
        (r###"\det J_F"###, r###"det J_F"###),
        (r###"(-\tfrac14, 0, 0)"###, r###"(-1/4, 0, 0)"###),
        (r###"u = xy"###, r###"u = xy"###),
        (r###"t = x^2z"###, r###"t = x²z"###),
        (r###"x \neq 0"###, r###"x ≠ 0"###),
        (r###"f_1^{\text{ut}}, f_2^{\text{ut}}"###, r###"f₁ᵘᵗ, f₂ᵘᵗ"###),
        (r###"u,t"###, r###"u,t"###),
        (r###"x"###, r###"x"###),
        (r###"x, x^2"###, r###"x, x²"###),
        (r###"\mathbb{C}^n \to \mathbb{C}^n"###, r###"ℂⁿ → ℂⁿ"###),
        (r###"n \geq 2"###, r###"n ≥ 2"###),
        (r###"\mathbb{P}^3"###, r###"ℙ³"###),
        (r###"e^{i\pi}+1=0"###, r###"e^(iπ)+1 = 0"###),
        (
            "\\boxed{\n\\mathcal{Z}(\\beta)\n=\n\\int_{\\mathcal M}\n\\exp\\!\\left(\n-\\beta\\left[\n\\frac12 g^{ij}(x)\\,\\partial_i\\phi\\,\\partial_j\\phi\n+V(\\phi)\n\\right]\\right)\n\\mathcal D\\phi\n}",
            r###"[Z(β) = ∫_M exp( -β[ 1/2 gⁱʲ(x) ∂ᵢϕ ∂ⱼϕ +V(ϕ) ]) Dϕ]"###,
        ),
        (
            "\\begin{aligned}\n\\nabla_\\mu T^{\\mu\\nu}\n&=\n\\frac{1}{\\sqrt{-g}}\n\\partial_\\mu\\!\\left(\\sqrt{-g}\\,T^{\\mu\\nu}\\right)\n+\\Gamma^\\nu_{\\mu\\lambda}T^{\\mu\\lambda}\n=0, \\\\[4pt]\nR_{\\mu\\nu}-\\frac12 Rg_{\\mu\\nu}+\\Lambda g_{\\mu\\nu}\n&=\n\\frac{8\\pi G}{c^4}T_{\\mu\\nu}.\n\\end{aligned}",
            "∇_μ T^(μν) = 1/(√(-g)) ∂_μ(√(-g) T^(μν)) +Γ^ν_(μλ)T^(μλ) = 0,\nR_(μν)-1/2 Rg_(μν)+Λ g_(μν) = (8π G)/(c⁴)T_(μν).",
        ),
        (
            r###"x=\frac{-b\pm\sqrt{b^2-4ac}}{2a}"###,
            r###"x = (-b±√(b²-4ac))/(2a)"###,
        ),
        (
            r###"\int_0^\infty e^{-x^2}\,dx=\frac{\sqrt{\pi}}{2}"###,
            r###"∫₀^∞ e^(-x²) dx = (√π)/2"###,
        ),
        (
            r###"e^{i\theta}=\cos\theta+i\sin\theta"###,
            r###"e^(iθ) = cos θ+i sin θ"###,
        ),
        (
            r###"\sum_{n=1}^{\infty}\frac{1}{n^2}=\frac{\pi^2}{6}"###,
            r###"∑ₙ₌₁^∞1/(n²) = π²/6"###,
        ),
        (
            r###"\lim_{x\to 0}\frac{\sin x}{x}=1"###,
            r###"lim[x→0] (sin x)/x = 1"###,
        ),
        (
            "\\lim_{n\\to\\infty}\n\\left(1+\\frac{1}{n}\\right)^n=e",
            r###"lim[n→∞] (1+1/n)ⁿ = e"###,
        ),
        (
            "\\int_0^1 \\frac{x^2}{1+x^3}\\,dx\n=\\frac{1}{3}\\ln 2",
            r###"∫₀¹ x²/(1+x³) dx = 1/3 ln 2"###,
        ),
        (
            "\\sum_{k=1}^{n}\\frac{k}{k+1}\n=n+1-H_{n+1}",
            r###"∑ₖ₌₁ⁿk/(k+1) = n+1-Hₙ₊₁"###,
        ),
        (
            "\\frac{\n  \\displaystyle \\frac{x^2+1}{x-1}\n  -\n  \\displaystyle \\frac{2x}{x+1}\n}{\n  \\displaystyle \\frac{x}{x^2-1}\n}",
            r###"((x²+1)/(x-1) - 2x/(x+1))/(x/(x²-1))"###,
        ),
        (
            "\\lim_{x\\to 0}\n\\frac{\n  \\displaystyle \\frac{\\sin x}{x}-1\n}{\n  \\displaystyle \\frac{e^x-1}{x}-1\n}\n=0",
            r###"lim[x→0] ((sin x)/x-1)/((eˣ-1)/x-1) = 0"###,
        ),
        (
            "\\frac{\n  1+\\displaystyle\\frac{1}{1+\\frac{1}{x}}\n}{\n  1-\\displaystyle\\frac{1}{1-\\frac{1}{x}}\n}",
            r###"(1+1/(1+1/x))/(1-1/(1-1/x))"###,
        ),
        (
            "\\sum_{n=1}^{\\infty}\n\\frac{\n  \\displaystyle \\frac{1}{n}-\\frac{1}{n+1}\n}{\n  \\displaystyle 1+\\frac{1}{n^2}\n}",
            r###"∑ₙ₌₁^∞ (1/n-1/(n+1))/(1+1/(n²))"###,
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(render_latex(source, false).as_deref(), Some(*expected), "{source}");
    }
}
