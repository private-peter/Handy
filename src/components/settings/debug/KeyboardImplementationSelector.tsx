import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { type } from "@tauri-apps/plugin-os";
import { SettingContainer } from "../../ui/SettingContainer";
import { Dropdown, type DropdownOption } from "../../ui/Dropdown";
import { Button } from "../../ui/Button";
import { useSettings } from "../../../hooks/useSettings";
import { commands } from "@/bindings";
import { notify } from "@/stores/noticeStore";

const IS_LINUX = type() === "linux";

interface KeyboardImplementationSelectorProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const KeyboardImplementationSelector: React.FC<
  KeyboardImplementationSelectorProps
> = ({ descriptionMode = "tooltip", grouped = false }) => {
  const { t } = useTranslation();
  const { getSetting, isUpdating, refreshSettings } = useSettings();
  const [isChanging, setIsChanging] = useState(false);
  const [pasteFeedback, setPasteFeedback] = useState<string | null>(null);

  useEffect(() => {
    if (!pasteFeedback) return;
    const timeout = window.setTimeout(() => setPasteFeedback(null), 5000);
    return () => window.clearTimeout(timeout);
  }, [pasteFeedback]);
  const options: DropdownOption[] = [
    { value: "tauri", label: "Tauri Global Shortcut" },
    { value: "handy_keys", label: "Handy Keys" },
    ...(IS_LINUX
      ? [
          {
            value: "portal",
            label: t("settings.debug.keyboardImplementation.portalLabel"),
          },
          { value: "none", label: "None" },
        ]
      : []),
  ];
  const currentImplementation =
    getSetting("keyboard_implementation") ?? "tauri";
  const isPortal = IS_LINUX && currentImplementation === "portal";
  const controlsDisabled = isChanging || isUpdating("keyboard_implementation");

  const handleSelect = async (value: string, retry = false) => {
    if ((!retry && value === currentImplementation) || isChanging) return;

    setIsChanging(true);
    setPasteFeedback(null);
    try {
      const result = await commands.changeKeyboardImplementationSetting(value);

      if (result.status === "error") {
        console.error(
          "Failed to update keyboard implementation:",
          result.error,
        );
        notify({ title: String(result.error) });
        await refreshSettings();
        return;
      }

      // If any bindings were reset due to incompatibility, notify the user
      if (result.data.reset_bindings.length > 0) {
        notify({
          title: t("settings.debug.keyboardImplementation.bindingsReset"),
          level: "warning",
        });
      }

      await refreshSettings();
    } catch (error) {
      console.error("Failed to update keyboard implementation:", error);
      notify({ title: String(error) });
    } finally {
      setIsChanging(false);
    }
  };

  const handlePasting = async (action: "setup" | "stop") => {
    if (controlsDisabled) return;

    setIsChanging(true);
    setPasteFeedback(null);
    const failureTitle = t(
      action === "setup"
        ? "settings.debug.keyboardImplementation.portalPasteSetupFailed"
        : "settings.debug.keyboardImplementation.portalPasteStopFailed",
    );

    try {
      const result =
        action === "setup"
          ? await commands.setupPortalPasting()
          : await commands.stopPortalPasting();

      if (result.status === "error") {
        notify({ title: failureTitle, detail: String(result.error) });
        return;
      }

      setPasteFeedback(
        t(
          action === "setup"
            ? "settings.debug.keyboardImplementation.portalPasteSetupComplete"
            : "settings.debug.keyboardImplementation.portalPasteStopped",
        ),
      );
    } catch (error) {
      notify({ title: failureTitle, detail: String(error) });
    } finally {
      setIsChanging(false);
    }
  };

  return (
    <SettingContainer
      title={t("settings.debug.keyboardImplementation.title")}
      description={t(
        currentImplementation === "portal"
          ? "settings.debug.keyboardImplementation.portalDescription"
          : "settings.debug.keyboardImplementation.description",
      )}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout={isPortal ? "stacked" : "horizontal"}
    >
      <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-end gap-2">
          <Dropdown
            options={options}
            selectedValue={currentImplementation}
            onSelect={handleSelect}
            disabled={controlsDisabled}
          />
          {isPortal && (
            <Button
              type="button"
              variant="secondary"
              size="sm"
              onClick={() => void handleSelect("portal", true)}
              disabled={controlsDisabled}
            >
              {t("settings.debug.keyboardImplementation.portalSetup")}
            </Button>
          )}
        </div>
        {isPortal && (
          <div className="space-y-2 border-t border-line pt-3">
            <p className="text-xs text-ink-3">
              {t(
                "settings.debug.keyboardImplementation.portalPasteDescription",
              )}
            </p>
            <div className="flex flex-wrap items-center justify-end gap-2">
              <Button
                type="button"
                variant="secondary"
                size="sm"
                onClick={() => void handlePasting("setup")}
                disabled={controlsDisabled}
              >
                {t("settings.debug.keyboardImplementation.portalPasteSetup")}
              </Button>
              <Button
                type="button"
                variant="secondary"
                size="sm"
                onClick={() => void handlePasting("stop")}
                disabled={controlsDisabled}
              >
                {t("settings.debug.keyboardImplementation.portalPasteStop")}
              </Button>
            </div>
            {pasteFeedback && (
              <p role="status" className="text-xs text-ink-3">
                {pasteFeedback}
              </p>
            )}
          </div>
        )}
      </div>
    </SettingContainer>
  );
};
