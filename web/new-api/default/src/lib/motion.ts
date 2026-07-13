/*
Copyright (C) 2023-2026 QuantumNous

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as
published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License
along with this program. If not, see <https://www.gnu.org/licenses/>.

For commercial licensing, please contact support@quantumnous.com
*/
import type { Transition, Variants } from 'motion/react'

/**
 * Apple-style fluid motion (WWDC "Designing Fluid Interfaces").
 * Default springs are critically damped (no overshoot). Bounce is reserved
 * for momentum-driven gestures (flick / sheet release).
 *
 * Motion maps: bounce≈0 → damping 1.0; bounce≈0.2 → damping ~0.8
 * duration maps to Apple "response" (not a fixed CSS duration).
 */
const SPRING = {
  /** Move / reposition — damping 1.0, response 0.4 */
  default: { type: 'spring', bounce: 0, duration: 0.4 } as const,
  /** Snappy UI settle — damping 1.0, response 0.3 */
  snappy: { type: 'spring', bounce: 0, duration: 0.3 } as const,
  /** Soft settle for larger surfaces */
  soft: { type: 'spring', bounce: 0, duration: 0.5 } as const,
  /** Drawer / sheet with gesture momentum — damping ~0.8, response 0.3 */
  sheet: { type: 'spring', bounce: 0.2, duration: 0.3 } as const,
  /** Flick / throw — slight overshoot only when velocity exists */
  momentum: { type: 'spring', bounce: 0.2, duration: 0.4 } as const,
} as const

/** Strong ease-out (Emil) — instant response, soft settle. */
const EASE_OUT = [0.23, 1, 0.32, 1] as const
/** Softer ease-out for larger surfaces / page travel. */
const EASE_OUT_SOFT = [0.33, 1, 0.68, 1] as const
/** On-screen morph / reposition (ease-in-out). */
const EASE_IN_OUT = [0.77, 0, 0.175, 1] as const
/** Inverse of soft ease-out for reversible exit paths. */
const EASE_IN_MIRROR = [0.32, 0, 0.67, 0] as const

const DURATION = {
  instant: 0,
  press: 0.1,
  fast: 0.15,
  normal: 0.2,
  panel: 0.25,
  slow: 0.3,
} as const

/** Cross-fade for prefers-reduced-motion (no slides / springs). */
const REDUCED: Transition = {
  duration: DURATION.normal,
  ease: EASE_OUT,
}

export const MOTION_EASING = {
  out: EASE_OUT,
  outSoft: EASE_OUT_SOFT,
  inOut: EASE_IN_OUT,
  inMirror: EASE_IN_MIRROR,
} as const

export const MOTION_DURATION = DURATION

export const MOTION_TRANSITION: Record<string, Transition> = {
  default: SPRING.default,
  fast: SPRING.snappy,
  slow: SPRING.soft,
  spring: SPRING.default,
  sheet: SPRING.sheet,
  momentum: SPRING.momentum,
  press: { duration: DURATION.press, ease: EASE_OUT },
  fade: { duration: DURATION.normal, ease: EASE_OUT },
  panel: { duration: DURATION.panel, ease: EASE_OUT },
  reduced: REDUCED,
  none: { duration: DURATION.instant },
  /** Inverse easing for reversible exit paths (spatial consistency). */
  exitEase: { duration: DURATION.fast, ease: EASE_IN_MIRROR },
}

/**
 * Project resting position from release velocity (Apple exponential decay).
 * decelerationRate ≈ 0.998 normal scroll; 0.99 snappier.
 */
export function projectMomentum(
  initialVelocity: number,
  decelerationRate = 0.998
): number {
  return (
    (initialVelocity / 1000) * (decelerationRate / (1 - decelerationRate))
  )
}

/** Rubber-band past a bound — progressive resistance, not a hard stop. */
export function rubberband(
  overshoot: number,
  dimension: number,
  constant = 0.55
): number {
  return (
    (overshoot * dimension * constant) /
    (dimension + constant * Math.abs(overshoot))
  )
}

export const MOTION_VARIANTS = {
  pageEnter: {
    initial: { opacity: 0, y: 6 },
    animate: { opacity: 1, y: 0 },
    exit: { opacity: 0, y: -4 },
  },
  /** Reduced-motion / materialize: opacity only, no travel. */
  pageEnterReduced: {
    initial: { opacity: 0 },
    animate: { opacity: 1 },
    exit: { opacity: 0 },
  },
  fadeIn: {
    initial: { opacity: 0 },
    animate: { opacity: 1 },
    exit: { opacity: 0 },
  },
  scaleIn: {
    initial: { opacity: 0, scale: 0.96 },
    animate: { opacity: 1, scale: 1 },
    exit: { opacity: 0, scale: 0.96 },
  },
  /** Sheet/dialog: scale + slight blur materialize (not bare fade). */
  materialize: {
    initial: { opacity: 0, scale: 0.97, filter: 'blur(4px)' },
    animate: { opacity: 1, scale: 1, filter: 'blur(0px)' },
    exit: { opacity: 0, scale: 0.98, filter: 'blur(2px)' },
  },
  slideUp: {
    initial: { opacity: 0, y: 16 },
    animate: { opacity: 1, y: 0 },
    exit: { opacity: 0, y: 16 },
  },
  slideDown: {
    initial: { opacity: 0, y: -16 },
    animate: { opacity: 1, y: 0 },
    exit: { opacity: 0, y: -16 },
  },
  tableRow: {
    initial: { opacity: 0, y: 4 },
    animate: { opacity: 1, y: 0 },
  },
  cardItem: {
    initial: { opacity: 0, y: 10, scale: 0.99 },
    animate: { opacity: 1, y: 0, scale: 1 },
  },
  sidebarSlide: {
    initial: { opacity: 0, x: -8 },
    animate: { opacity: 1, x: 0 },
    exit: { opacity: 0, x: -8 },
  },
} as const

export const STAGGER_VARIANTS: Variants = {
  initial: {},
  animate: { transition: { staggerChildren: 0.04 } },
}

export const STAGGER_ITEM_VARIANTS: Variants = {
  initial: { opacity: 0, y: 8 },
  animate: { opacity: 1, y: 0, transition: MOTION_TRANSITION.default },
}

export const TABLE_STAGGER_VARIANTS: Variants = {
  initial: {},
  animate: { transition: { staggerChildren: 0.03 } },
}

export const TABLE_ROW_VARIANTS: Variants = {
  initial: { opacity: 0, y: 4 },
  animate: { opacity: 1, y: 0, transition: MOTION_TRANSITION.fast },
}

export const CARD_STAGGER_VARIANTS: Variants = {
  initial: {},
  animate: { transition: { staggerChildren: 0.05 } },
}

export const CARD_ITEM_VARIANTS: Variants = {
  initial: { opacity: 0, y: 10, scale: 0.99 },
  animate: {
    opacity: 1,
    y: 0,
    scale: 1,
    transition: MOTION_TRANSITION.default,
  },
}

export const SIDEBAR_STAGGER_VARIANTS: Variants = {
  initial: {},
  animate: { transition: { staggerChildren: 0.03, delayChildren: 0.05 } },
}

export const SIDEBAR_ITEM_VARIANTS: Variants = {
  initial: { opacity: 0, x: -8 },
  animate: { opacity: 1, x: 0, transition: MOTION_TRANSITION.fast },
}
