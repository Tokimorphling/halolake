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
import { zodResolver } from '@hookform/resolvers/zod'
import { type FormEvent, useEffect, useState } from 'react'
import { useForm } from 'react-hook-form'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
import { z } from 'zod'

import {
  SideDrawerSection,
  sideDrawerContentClassName,
  sideDrawerFooterClassName,
  sideDrawerFormClassName,
  sideDrawerHeaderClassName,
} from '@/components/drawer-layout'
import { Button } from '@/components/ui/button'
import {
  Form,
  FormControl,
  FormDescription,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import {
  Sheet,
  SheetClose,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'

import { createProxy, getProxy, updateProxy } from '../api'
import { ERROR_MESSAGES, PROXY_STATUS, SUCCESS_MESSAGES } from '../constants'
import type { Proxy } from '../types'
import { useProxies } from './proxies-provider'

const proxyFormSchema = z.object({
  name: z.string().min(1, 'Name is required'),
  url: z
    .string()
    .min(1, 'URL is required')
    .refine(
      (value) => {
        try {
          const parsed = new URL(value)
          return ['http:', 'https:', 'socks5:', 'socks5h:'].includes(
            parsed.protocol
          )
        } catch {
          return false
        }
      },
      {
        message:
          'Invalid proxy URL (allowed: http, https, socks5, socks5h)',
      }
    ),
  status: z.boolean(),
  remark: z.string(),
})

type ProxyFormValues = z.infer<typeof proxyFormSchema>

const DEFAULT_VALUES: ProxyFormValues = {
  name: '',
  url: '',
  status: true,
  remark: '',
}

type ProxiesMutateDrawerProps = {
  open: boolean
  onOpenChange: (open: boolean) => void
  currentRow?: Proxy
}

export function ProxiesMutateDrawer({
  open,
  onOpenChange,
  currentRow,
}: ProxiesMutateDrawerProps) {
  const { t } = useTranslation()
  const isUpdate = !!currentRow
  const { triggerRefresh } = useProxies()
  const [isSubmitting, setIsSubmitting] = useState(false)

  const form = useForm<ProxyFormValues>({
    resolver: zodResolver(proxyFormSchema),
    defaultValues: DEFAULT_VALUES,
  })

  useEffect(() => {
    if (open && isUpdate && currentRow) {
      void getProxy(currentRow.id)
        .then((result) => {
          if (result.success && result.data) {
            form.reset({
              name: result.data.name,
              url: result.data.url,
              status: result.data.status === PROXY_STATUS.ENABLED,
              remark: result.data.remark || '',
            })
          }
        })
        .catch(() => {
          /* best-effort form hydrate */
        })
    } else if (open && !isUpdate) {
      form.reset(DEFAULT_VALUES)
    }
  }, [open, isUpdate, currentRow, form])

  const onSubmit = async (data: ProxyFormValues) => {
    setIsSubmitting(true)
    try {
      const payload = {
        name: data.name.trim(),
        url: data.url.trim(),
        status: data.status ? PROXY_STATUS.ENABLED : PROXY_STATUS.DISABLED,
        remark: data.remark.trim(),
      }
      const result =
        isUpdate && currentRow
          ? await updateProxy({ ...payload, id: currentRow.id })
          : await createProxy(payload)
      if (result.success) {
        toast.success(
          t(
            isUpdate
              ? SUCCESS_MESSAGES.PROXY_UPDATED
              : SUCCESS_MESSAGES.PROXY_CREATED
          )
        )
        onOpenChange(false)
        triggerRefresh()
      } else {
        toast.error(result.message || t(ERROR_MESSAGES.SAVE_FAILED))
      }
    } finally {
      setIsSubmitting(false)
    }
  }

  const handleSubmit = (event: FormEvent<HTMLFormElement>) => {
    void form.handleSubmit(onSubmit)(event)
  }

  return (
    <Sheet
      open={open}
      onOpenChange={(v) => {
        onOpenChange(v)
        if (!v) form.reset()
      }}
    >
      <SheetContent className={sideDrawerContentClassName('sm:max-w-[520px]')}>
        <SheetHeader className={sideDrawerHeaderClassName()}>
          <SheetTitle>
            {isUpdate ? t('Update Proxy') : t('Add Proxy')}
          </SheetTitle>
          <SheetDescription>
            {t(
              'Upstream egress proxy for channels. socks5 is upgraded to socks5h (remote DNS).'
            )}
          </SheetDescription>
        </SheetHeader>
        <Form {...form}>
          <form
            id='proxy-form'
            onSubmit={handleSubmit}
            className={sideDrawerFormClassName()}
          >
            <SideDrawerSection>
              <FormField
                control={form.control}
                name='name'
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>{t('Name')}</FormLabel>
                    <FormControl>
                      <Input placeholder={t('us-east')} {...field} />
                    </FormControl>
                    <FormMessage />
                  </FormItem>
                )}
              />
              <FormField
                control={form.control}
                name='url'
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>{t('URL')}</FormLabel>
                    <FormControl>
                      <Input
                        placeholder='socks5://user:pass@127.0.0.1:1080'
                        {...field}
                      />
                    </FormControl>
                    <FormDescription>
                      {t(
                        'http, https, socks5, or socks5h. socks5 becomes socks5h on save.'
                      )}
                    </FormDescription>
                    <FormMessage />
                  </FormItem>
                )}
              />
              <FormField
                control={form.control}
                name='status'
                render={({ field }) => (
                  <FormItem className='flex items-center justify-between'>
                    <div className='space-y-0.5'>
                      <FormLabel>{t('Enabled')}</FormLabel>
                      <FormDescription>
                        {t(
                          'Disabled proxies are not resolved for channel traffic.'
                        )}
                      </FormDescription>
                    </div>
                    <FormControl>
                      <Switch
                        checked={field.value}
                        onCheckedChange={field.onChange}
                      />
                    </FormControl>
                  </FormItem>
                )}
              />
              <FormField
                control={form.control}
                name='remark'
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>{t('Remark')}</FormLabel>
                    <FormControl>
                      <Textarea
                        rows={2}
                        placeholder={t('Optional notes')}
                        {...field}
                      />
                    </FormControl>
                    <FormMessage />
                  </FormItem>
                )}
              />
            </SideDrawerSection>
          </form>
        </Form>
        <SheetFooter className={sideDrawerFooterClassName()}>
          <SheetClose render={<Button variant='outline' />}>
            {t('Cancel')}
          </SheetClose>
          <Button type='submit' form='proxy-form' disabled={isSubmitting}>
            {isSubmitting ? t('Saving...') : t('Save')}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  )
}
