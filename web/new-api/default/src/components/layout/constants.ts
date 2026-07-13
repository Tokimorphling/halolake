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
/**
 * Layout constants and configurations
 */

/**
 * Animation variants for mobile drawer
 */
export const MOBILE_DRAWER_ANIMATION = {
  overlay: {
    hidden: { opacity: 0 },
    visible: { opacity: 1 },
    exit: { opacity: 0 },
  },
  drawer: {
    hidden: { opacity: 0, y: 100 },
    visible: {
      opacity: 1,
      y: 0,
      rotate: 0,
      transition: {
        type: 'spring',
        bounce: 0.15,
        duration: 0.35,
        staggerChildren: 0.03,
      },
    },
    exit: {
      opacity: 0,
      y: 100,
      transition: { type: 'spring', bounce: 0, duration: 0.25 },
    },
  },
  menuItem: {
    hidden: { opacity: 0 },
    visible: { opacity: 1 },
  },
} as const

/**
 * Mobile drawer configuration
 */
export const MOBILE_DRAWER_CONFIG = {
  overlayTransitionDuration: 0.2,
  drawerClassName:
    'app-material-chrome fixed inset-x-0 bottom-3 z-50 mx-auto w-[95%] rounded-2xl border border-[color:var(--material-chrome-border)] p-4 shadow-[0_8px_40px_oklch(0_0_0/0.16)] md:hidden',
  overlayClassName: 'fixed inset-0 z-40 bg-black/30 backdrop-blur-sm',
} as const
