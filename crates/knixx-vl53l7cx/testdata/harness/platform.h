/*
 * Platform layer for running the ST ULD driver on the host against a fake
 * sensor: every I2C transaction and wait is logged, see harness.c. Only the
 * motion indicator output is enabled, as in knixx-vl53l7cx.
 */
#ifndef _PLATFORM_H_
#define _PLATFORM_H_
#pragma once

#include <stdint.h>
#include <string.h>

typedef struct
{
	uint16_t address;
} VL53L7CX_Platform;

#define VL53L7CX_NB_TARGET_PER_ZONE		1U

#define VL53L7CX_DISABLE_AMBIENT_PER_SPAD
#define VL53L7CX_DISABLE_NB_SPADS_ENABLED
#define VL53L7CX_DISABLE_NB_TARGET_DETECTED
#define VL53L7CX_DISABLE_SIGNAL_PER_SPAD
#define VL53L7CX_DISABLE_RANGE_SIGMA_MM
#define VL53L7CX_DISABLE_DISTANCE_MM
#define VL53L7CX_DISABLE_REFLECTANCE_PERCENT
#define VL53L7CX_DISABLE_TARGET_STATUS

uint8_t VL53L7CX_RdByte(VL53L7CX_Platform *p_platform,
		uint16_t RegisterAdress, uint8_t *p_value);
uint8_t VL53L7CX_WrByte(VL53L7CX_Platform *p_platform,
		uint16_t RegisterAdress, uint8_t value);
uint8_t VL53L7CX_RdMulti(VL53L7CX_Platform *p_platform,
		uint16_t RegisterAdress, uint8_t *p_values, uint32_t size);
uint8_t VL53L7CX_WrMulti(VL53L7CX_Platform *p_platform,
		uint16_t RegisterAdress, uint8_t *p_values, uint32_t size);
void VL53L7CX_SwapBuffer(uint8_t *buffer, uint16_t size);
uint8_t VL53L7CX_WaitMs(VL53L7CX_Platform *p_platform, uint32_t TimeMs);

#endif	// _PLATFORM_H_
