import * as React from "react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import { ScrollArea } from "@/components/ui/scroll-area"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Slider } from "@/components/ui/slider"
import { Switch } from "@/components/ui/switch"
import {
  engine,
  onEngineEvent,
  type AllWindowsOpacityStatus,
  type Config,
  type ScrollModifier,
  type ScrollTransparencyStatus,
  type WindowInfo,
} from "@/lib/engine"
import type { I18n } from "@/lib/use-i18n"
import { cn } from "@/lib/utils"

const FULLY_OPAQUE = 255

/** The floor shared by every opacity slider, and by the engine. */
const MIN_ALPHA = 20

/** What the all-windows slider starts at before it has been set: 80%. */
const DEFAULT_ALL_WINDOWS_ALPHA = 204

/** How long the all-windows slider rests before the desktop is re-faded. */
const LIVE_DELAY_MS = 120

const percent = (alpha: number) => Math.round((alpha / FULLY_OPAQUE) * 100)

/** Display names for the modifiers the engine accepts. */
const MODIFIER_LABELS: Record<ScrollModifier, string> = {
  alt: "Alt",
  ctrl: "Ctrl",
  shift: "Shift",
  win: "Win",
}

interface Props {
  config: Config
  i18n: I18n
  /** Bumped by the shell after each save, when the engine re-syncs the hook. */
  savedAt: number
  onChange: <S extends keyof Config, K extends keyof Config[S]>(
    section: S,
    key: K,
    value: Config[S][K],
  ) => void
}

export function TransparencyTab({ config, i18n, savedAt, onChange }: Props) {
  const { t } = i18n
  const [windows, setWindows] = React.useState<WindowInfo[]>([])
  const [selected, setSelected] = React.useState<WindowInfo | null>(null)
  const [alpha, setAlpha] = React.useState(FULLY_OPAQUE)
  const [saved, setSaved] = React.useState<Record<string, number>>({})
  const [scroll, setScroll] = React.useState<ScrollTransparencyStatus | null>(null)
  const [allWindows, setAllWindows] = React.useState<AllWindowsOpacityStatus | null>(null)
  const liveTimer = React.useRef<number | undefined>(undefined)

  const refresh = React.useCallback(async () => {
    try {
      const [list, settings] = await Promise.all([
        engine.listWindows(),
        engine.getOpacitySettings(),
      ])
      setWindows(list.windows)
      setSaved(settings.settings)
    } catch (e) {
      toast.error((e as Error).message)
    }
  }, [])

  React.useEffect(() => {
    void refresh()
  }, [refresh])

  // Re-read after every save: the engine installs or removes the hook there, and
  // the badge would otherwise keep claiming whatever was true at mount.
  React.useEffect(() => {
    void engine.scrollTransparencyStatus().then(setScroll).catch(() => {})
    void engine.allWindowsOpacityStatus().then(setAllWindows).catch(() => {})
  }, [savedAt])

  // A pending live update must not land after the screen has gone.
  React.useEffect(() => () => window.clearTimeout(liveTimer.current), [])

  React.useEffect(() => {
    // Scrolling edits the same opacity map this screen shows, so follow it live
    // rather than letting the list go stale behind the user's back.
    const unlisten = onEngineEvent((event) => {
      if (event.event !== "transparency_changed") return
      const { process, alpha: next } = event.data
      setSaved((prev) => ({ ...prev, [process]: next }))
      setSelected((sel) => {
        if (sel?.process === process) setAlpha(next)
        return sel
      })
    })
    return () => {
      void unlisten.then((fn) => fn())
    }
  }, [])

  const scrollEnabled = config.hotkeys.scroll_enabled ?? false
  const scrollModifier = config.hotkeys.scroll_modifier ?? "alt"
  const scrollUnavailable = scroll !== null && !scroll.available

  const allEnabled = config.transparency?.all_windows ?? false
  const allAlpha = config.transparency?.all_windows_alpha ?? DEFAULT_ALL_WINDOWS_ALPHA
  const allUnavailable = allWindows !== null && !allWindows.available

  // Edits the draft like every other setting, and also shows it on the real desktop
  // straight away — the point of an opacity slider is seeing the windows change.
  // Saving is what makes it stick, and what brings it back after a restart.
  function onAllWindowsChange(next: { all_windows: boolean; all_windows_alpha: number }) {
    onChange("transparency", "all_windows", next.all_windows)
    onChange("transparency", "all_windows_alpha", next.all_windows_alpha)
    window.clearTimeout(liveTimer.current)
    liveTimer.current = window.setTimeout(() => {
      void engine
        .syncAllWindowsOpacity({ transparency: next })
        .then(setAllWindows)
        .catch((e) => toast.error((e as Error).message))
    }, LIVE_DELAY_MS)
  }

  function select(win: WindowInfo) {
    setSelected(win)
    setAlpha(saved[win.process] ?? FULLY_OPAQUE)
  }

  // Applied live while dragging so the user sees the result on the real window;
  // persisting is a separate, explicit step.
  function onAlphaChange(next: number) {
    setAlpha(next)
    if (selected) void engine.setWindowOpacity(selected.hwnd, next).catch(() => {})
  }

  async function persist() {
    if (!selected) return
    try {
      const next = { ...saved, [selected.process]: alpha }
      await engine.saveOpacitySettings(next)
      setSaved(next)
      toast.success(t("saved"))
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <Card>
        <CardHeader>
          <CardTitle>{t("all_windows")}</CardTitle>
          {allWindows?.running && (
            <CardAction>
              <Badge>{t("active")}</Badge>
            </CardAction>
          )}
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <p className="text-sm text-muted-foreground">{t("all_windows_hint")}</p>

          <div className="flex items-center justify-between">
            <Label htmlFor="all-windows-enabled" className="font-normal">
              {t("all_windows_enable")}
            </Label>
            <Switch
              id="all-windows-enabled"
              checked={allEnabled}
              disabled={allUnavailable}
              onCheckedChange={(v) =>
                onAllWindowsChange({ all_windows: v, all_windows_alpha: allAlpha })
              }
            />
          </div>

          <div className="flex items-center gap-4">
            <Slider
              min={MIN_ALPHA}
              max={FULLY_OPAQUE}
              step={1}
              value={[allAlpha]}
              disabled={!allEnabled || allUnavailable}
              onValueChange={(v) =>
                onAllWindowsChange({
                  all_windows: allEnabled,
                  all_windows_alpha: Array.isArray(v) ? v[0] : v,
                })
              }
            />
            <span className="w-16 text-right font-mono text-sm">{percent(allAlpha)}%</span>
          </div>

          {allWindows?.running && (
            <p className="text-xs text-muted-foreground">
              {t("all_windows_faded", { n: allWindows.faded })}
            </p>
          )}
          <p className="text-xs text-muted-foreground">{t("all_windows_persist_hint")}</p>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex-row items-center justify-between">
          <CardTitle>{t("scroll_transparency")}</CardTitle>
          {/* The switch is what the config asks for; this says what is actually
              installed, which is the difference that matters when it fails. */}
          {scroll?.running && <Badge>{t("active")}</Badge>}
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <p className="text-sm text-muted-foreground">
            {t("scroll_transparency_hint", {
              modifier: MODIFIER_LABELS[scrollModifier as ScrollModifier] ?? scrollModifier,
            })}
          </p>

          {scrollUnavailable && (
            <p className="text-sm text-destructive">{t("scroll_unavailable")}</p>
          )}

          <div className="flex items-center justify-between">
            <Label htmlFor="scroll-enabled" className="font-normal">
              {t("scroll_enable")}
            </Label>
            <Switch
              id="scroll-enabled"
              checked={scrollEnabled}
              disabled={scrollUnavailable}
              onCheckedChange={(v) => onChange("hotkeys", "scroll_enabled", v)}
            />
          </div>

          <div className="flex flex-col gap-1.5">
            <Label className="font-normal text-muted-foreground">
              {t("scroll_modifier_label")}
            </Label>
            <Select
              value={scrollModifier}
              disabled={!scrollEnabled || scrollUnavailable}
              onValueChange={(v) =>
                v && onChange("hotkeys", "scroll_modifier", v as ScrollModifier)
              }
            >
              <SelectTrigger>
                <SelectValue>
                  {(v) => MODIFIER_LABELS[String(v) as ScrollModifier] ?? String(v ?? "")}
                </SelectValue>
              </SelectTrigger>
              <SelectContent>
                {(scroll?.modifiers ?? (["alt", "ctrl", "shift", "win"] as ScrollModifier[])).map(
                  (m) => (
                    <SelectItem key={m} value={m}>
                      {MODIFIER_LABELS[m] ?? m}
                    </SelectItem>
                  ),
                )}
              </SelectContent>
            </Select>
          </div>

          <p className="text-xs text-muted-foreground">{t("scroll_takes_effect_on_save")}</p>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex-row items-center justify-between">
          <CardTitle>{t("open_windows")}</CardTitle>
          <Button variant="outline" size="sm" onClick={refresh}>
            {t("refresh")}
          </Button>
        </CardHeader>
        <CardContent>
          <ScrollArea className="h-64 rounded-md border">
            <div className="flex flex-col">
              {windows.length === 0 && (
                <p className="p-4 text-sm text-muted-foreground">{t("no_windows")}</p>
              )}
              {windows.map((win) => (
                <button
                  key={win.hwnd}
                  onClick={() => select(win)}
                  className={cn(
                    "flex flex-col items-start gap-0.5 border-b px-3 py-2 text-left last:border-b-0 hover:bg-accent",
                    selected?.hwnd === win.hwnd && "bg-accent",
                  )}
                >
                  <span className="truncate text-sm">{win.title || win.process}</span>
                  <span className="font-mono text-[11px] text-muted-foreground">
                    {win.process}
                    {saved[win.process] !== undefined && ` · ${saved[win.process]}`}
                  </span>
                </button>
              ))}
            </div>
          </ScrollArea>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t("opacity")}</CardTitle>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <p className="text-sm text-muted-foreground">
            {selected ? selected.title || selected.process : t("select_a_window")}
          </p>
          <div className="flex items-center gap-4">
            <Slider
              min={MIN_ALPHA}
              max={FULLY_OPAQUE}
              step={1}
              value={[alpha]}
              disabled={!selected}
              onValueChange={(v) => onAlphaChange(Array.isArray(v) ? v[0] : v)}
            />
            <span className="w-16 text-right font-mono text-sm">{percent(alpha)}%</span>
          </div>
          <div className="flex gap-2">
            <Button onClick={persist} disabled={!selected}>
              {t("save")}
            </Button>
            <Button
              variant="outline"
              disabled={!selected}
              onClick={() => onAlphaChange(FULLY_OPAQUE)}
            >
              {t("reset")}
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  )
}
