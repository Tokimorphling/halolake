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
import type { StatusBadgeProps } from '@/components/status-badge'

export const PROXY_STATUS = {
  DISABLED: 0,
  ENABLED: 1,
} as const

export const PROXY_STATUSES: Record<
  number,
  Pick<StatusBadgeProps, 'variant'> & { labelKey: string; value: number }
> = {
  [PROXY_STATUS.ENABLED]: {
    labelKey: 'Enabled',
    variant: 'success',
    value: PROXY_STATUS.ENABLED,
  },
  [PROXY_STATUS.DISABLED]: {
    labelKey: 'Disabled',
    variant: 'neutral',
    value: PROXY_STATUS.DISABLED,
  },
}

export const SUCCESS_MESSAGES = {
  PROXY_CREATED: 'Proxy created successfully',
  PROXY_UPDATED: 'Proxy updated successfully',
  PROXY_DELETED: 'Proxy deleted successfully',
  PROXY_ENABLED: 'Proxy enabled successfully',
  PROXY_DISABLED: 'Proxy disabled successfully',
} as const

export const ERROR_MESSAGES = {
  LOAD_FAILED: 'Failed to load proxies',
  SAVE_FAILED: 'Failed to save proxy',
  DELETE_FAILED: 'Failed to delete proxy',
} as const
