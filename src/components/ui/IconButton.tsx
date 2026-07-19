import type { ButtonHTMLAttributes, ReactNode } from "react";
import type { ButtonVariant } from "./Button";

export type IconButtonSize = "sm" | "md";

/** `active` = a toggled-on toolbar button (Claude-Desktop-style accent tint). */
export type IconButtonVariant = ButtonVariant | "active";

export interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: IconButtonVariant;
  size?: IconButtonSize;
  children?: ReactNode;
  "aria-label": string;
}

const BASE_CLASSES =
  "inline-flex items-center justify-center gap-1.5 rounded-md font-medium transition-colors duration-150 cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background";

const SIZE_CLASSES: Record<IconButtonSize, string> = {
  sm: "h-8 w-8",
  md: "h-9 w-9",
};

const VARIANT_CLASSES: Record<IconButtonVariant, string> = {
  primary: "bg-accent text-accent-foreground hover:bg-accent-hover",
  secondary:
    "bg-surface-2 text-foreground border border-border hover:bg-surface hover:border-border-strong",
  ghost: "text-muted hover:bg-surface-2 hover:text-foreground",
  active: "bg-accent-soft text-accent hover:bg-accent-soft hover:text-accent-hover",
  danger: "bg-danger text-danger-foreground hover:bg-danger-hover",
};

export function IconButton({
  variant = "ghost",
  size = "md",
  className,
  children,
  ...rest
}: IconButtonProps) {
  const classes = [BASE_CLASSES, SIZE_CLASSES[size], VARIANT_CLASSES[variant], className]
    .filter(Boolean)
    .join(" ");

  return (
    <button className={classes} {...rest}>
      {children}
    </button>
  );
}
