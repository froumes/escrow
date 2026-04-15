const THEME_MEDIA_QUERY = window.matchMedia("(prefers-color-scheme: dark)");
const THEME_VALUES = ["auto", "dark", "light"];

const readStoredTheme = () => {
    const value = localStorage.getItem("theme");
    return THEME_VALUES.includes(value) ? value : "auto";
};

const resolveTheme = (theme) => (theme === "auto" ? (THEME_MEDIA_QUERY.matches ? "dark" : "light") : theme);

const updateThemeColorMeta = () => {
    const metaThemeColor = document.querySelector('meta[name="theme-color"]');
    if (!metaThemeColor) return;
    const themeColor = getComputedStyle(document.documentElement).getPropertyValue("--theme-color").trim();
    if (themeColor) {
        metaThemeColor.setAttribute("content", themeColor);
    }
};

const applyTheme = (theme) => {
    const resolved = resolveTheme(theme);
    document.documentElement.dataset.theme = resolved;
    updateThemeColorMeta();
    return resolved;
};

const setThemePreference = (theme) => {
    localStorage.setItem("theme", theme);
    return applyTheme(theme);
};

const themePreference = readStoredTheme();
applyTheme(themePreference);

const navbar = document.getElementById("navbar");
const navToggle = document.querySelector("[data-nav-toggle]");
const navMenu = document.querySelector("[data-nav-menu]");
const navLinks = [...document.querySelectorAll(".nav-menu a[href^='#']")];
const tabs = [...document.querySelectorAll(".tab")];
const installPanels = [...document.querySelectorAll(".install-panel")];
const copyButtons = [...document.querySelectorAll(".copy-button")];
const revealElements = [...document.querySelectorAll("[data-reveal]")];
const themeToggle = document.querySelector("[data-theme-toggle]");
const sections = navLinks
    .map((link) => document.querySelector(link.getAttribute("href")))
    .filter(Boolean);

const updateThemeToggleLabel = () => {
    if (!themeToggle) return;
    const pref = readStoredTheme();
    const active = resolveTheme(pref);
    themeToggle.textContent = `Theme: ${pref.charAt(0).toUpperCase()}${pref.slice(1)}`;
    themeToggle.setAttribute("aria-pressed", String(pref !== "auto"));
    themeToggle.setAttribute("aria-label", `Theme mode ${pref}. Active ${active} theme.`);
};

const cycleTheme = () => {
    const current = readStoredTheme();
    const nextIndex = (THEME_VALUES.indexOf(current) + 1) % THEME_VALUES.length;
    setThemePreference(THEME_VALUES[nextIndex]);
    updateThemeToggleLabel();
};

themeToggle?.addEventListener("click", cycleTheme);

THEME_MEDIA_QUERY.addEventListener("change", () => {
    if (readStoredTheme() === "auto") {
        applyTheme("auto");
        updateThemeToggleLabel();
    }
});

updateThemeToggleLabel();

const setNavOpen = (isOpen) => {
    navbar.classList.toggle("nav-open", isOpen);
    navToggle?.setAttribute("aria-expanded", String(isOpen));
};

navToggle?.addEventListener("click", () => {
    setNavOpen(!navbar.classList.contains("nav-open"));
});

navLinks.forEach((link) => {
    link.addEventListener("click", () => setNavOpen(false));
});

document.addEventListener("click", (event) => {
    if (!navbar.contains(event.target)) {
        setNavOpen(false);
    }
});

const syncNavbarState = () => {
    navbar.classList.toggle("scrolled", window.scrollY > 20);
};

window.addEventListener("scroll", syncNavbarState, { passive: true });
syncNavbarState();

tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
        const targetId = `tab-${tab.dataset.tab}`;

        tabs.forEach((item) => item.classList.toggle("active", item === tab));
        installPanels.forEach((panel) => {
            panel.classList.toggle("active", panel.id === targetId);
        });
    });
});

copyButtons.forEach((button) => {
    button.addEventListener("click", async () => {
        const originalText = button.textContent;
        const text = button.dataset.copy?.replaceAll("&#10;", "\n") ?? "";

        try {
            await navigator.clipboard.writeText(text);
            button.textContent = "Copied";
        } catch {
            button.textContent = "Copy failed";
        }

        window.setTimeout(() => {
            button.textContent = originalText;
        }, 1800);
    });
});

if ("IntersectionObserver" in window) {
    const revealObserver = new IntersectionObserver(
        (entries, observer) => {
            entries.forEach((entry) => {
                if (!entry.isIntersecting) {
                    return;
                }

                entry.target.classList.add("is-visible");
                observer.unobserve(entry.target);
            });
        },
        { threshold: 0.14 }
    );

    revealElements.forEach((element) => revealObserver.observe(element));

    const sectionObserver = new IntersectionObserver(
        (entries) => {
            entries.forEach((entry) => {
                if (!entry.isIntersecting) {
                    return;
                }

                const activeId = `#${entry.target.id}`;

                navLinks.forEach((link) => {
                    link.classList.toggle("is-active", link.getAttribute("href") === activeId);
                });
            });
        },
        {
            rootMargin: "-35% 0px -45% 0px",
            threshold: 0
        }
    );

    sections.forEach((section) => sectionObserver.observe(section));
} else {
    revealElements.forEach((element) => element.classList.add("is-visible"));
}
