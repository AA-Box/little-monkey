import type { ButtonHTMLAttributes, ReactNode } from "react";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "danger";
export type ButtonSize = "xs" | "sm" | "md";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  children?: ReactNode;
}

const BASE_CLASSES =
  "inline-flex items-center justify-center gap-1.5 rounded-md font-medium transition-colors duration-150 cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background";

/** `xs` is for a crowded toolbar row — the padding is what gives, so the label
 *  still reads at full size. A size rather than a `className` override because
 *  utilities are ordered by value in the stylesheet, not by the order they are
 *  written in the attribute: `px-2.5` from here would win over a `px-1.5`
 *  passed in, and the override would silently do nothing. */
const SIZE_CLASSES: Record<ButtonSize, string> = {
  xs: "h-7 px-1 text-xs",
  sm: "h-8 px-2.5 text-xs",
  md: "h-9 px-3.5 text-sm",
};

const VARIANT_CLASSES: Record<ButtonVariant, string> = {
  primary: "bg-accent text-accent-foreground hover:bg-accent-hover",
  secondary:
    "bg-surface-2 text-foreground border border-border hover:bg-surface hover:border-border-strong",
  ghost: "text-muted hover:bg-surface-2 hover:text-foreground",
  danger: "bg-danger text-danger-foreground hover:bg-danger-hover",
};

export function Button({
  variant = "secondary",
  size = "md",
  className,
  children,
  ...rest
}: ButtonProps) {
  const classes = [BASE_CLASSES, SIZE_CLASSES[size], VARIANT_CLASSES[variant], className]
    .filter(Boolean)
    .join(" ");

  return (
    <button className={classes} {...rest}>
      {children}
    </button>
  );
}
