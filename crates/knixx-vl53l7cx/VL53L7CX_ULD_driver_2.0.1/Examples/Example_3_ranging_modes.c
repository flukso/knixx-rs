/*******************************************************************************
* Copyright (c) 2020, STMicroelectronics - All Rights Reserved
*
* This file is part of the VL53L7CX Ultra Lite Driver and 
* is licensed under terms that can be found in the LICENSE file
* in the root directory of this software component.
* If no LICENSE file comes with this software, it is provided AS-IS.
*
*******************************************************************************/

/***********************************/
/*    VL53L7CX ULD ranging mode    */
/***********************************/
/*
* This example shows the differences between ranging modes of VL53L7CX
* (mode continuous and autonomous). For both modes, it initializes the VL53L7CX
* ULD, set the mode, and starts a ranging to capture 10 frames.
*
* In this example, we also suppose that the number of target per zone is
* set to 1 , and all output are enabled (see file platform.h).
*/

#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include "vl53l7cx_api.h"

int example3(void)
{

	/*********************************/
	/*   VL53L7CX ranging variables  */
	/*********************************/

	uint8_t 				status, loop, isAlive, isReady, i;
	VL53L7CX_Configuration 	Dev;			/* Sensor configuration */
	VL53L7CX_ResultsData 	Results;		/* Results data from VL53L7CX */


	/*********************************/
	/*      Customer platform        */
	/*********************************/

	/* Fill the platform structure with customer's implementation. For this
	* example, only the I2C address is used.
	*/
	Dev.platform.address = VL53L7CX_DEFAULT_I2C_ADDRESS;

	/* (Optional) Reset sensor toggling PINs (see platform, not in API) */
	//VL53L7CX_Reset_Sensor(&(Dev.platform));

	/* (Optional) Set a new I2C address if the wanted address is different
	* from the default one (filled with 0x20 for this example).
	*/
	//status = vl53l7cx_set_i2c_address(&Dev, 0x20);

	
	/*********************************/
	/*   Power on sensor and init    */
	/*********************************/

	/* (Optional) Check if there is a VL53L7CX sensor connected */
	status = vl53l7cx_is_alive(&Dev, &isAlive);
	if(!isAlive || status)
	{
		printf("VL53L7CX not detected at requested address\n");
		return status;
	}

	/* (Mandatory) Init VL53L7CX sensor */
	status = vl53l7cx_init(&Dev);
	if(status)
	{
		printf("VL53L7CX ULD Loading failed\n");
		return status;
	}

	printf("VL53L7CX ULD ready ! (Version : %s)\n",
			VL53L7CX_API_REVISION);


	/*********************************/
	/*  Set ranging mode autonomous  */
	/*********************************/

	status = vl53l7cx_set_ranging_mode(&Dev, VL53L7CX_RANGING_MODE_AUTONOMOUS);
	if(status)
	{
		printf("vl53l7cx_set_ranging_mode failed, status %u\n", status);
		return status;
	}

	/* Using autonomous mode, the integration time can be updated (not possible
	 * using continuous) */
	status = vl53l7cx_set_integration_time_ms(&Dev, 20);

	/* Start a ranging session */
   	status = vl53l7cx_start_ranging(&Dev);
   	printf("Start ranging autonomous\n");

   	loop = 0;
   	while(loop < 10)
   	{
   		status = vl53l7cx_check_data_ready(&Dev, &isReady);
   		if(isReady)
   		{
   			vl53l7cx_get_ranging_data(&Dev, &Results);

   			/* As the sensor is set in 4x4 mode by default, we have a total
			 * of 16 zones to print. For this example, only the data of first zone are
			 * print */
   			printf("Print data no : %3u\n", Dev.streamcount);
   			for(i = 0; i < 16; i++)
   			{
				printf("Zone : %3d, Status : %3u, Distance : %4d mm\n",
					i,
					Results.target_status[VL53L7CX_NB_TARGET_PER_ZONE*i],
					Results.distance_mm[VL53L7CX_NB_TARGET_PER_ZONE*i]);
   			}
   			printf("\n");
   			loop++;
   		}

		/* Wait a few ms to avoid too high polling (function in platform
		 * file, not in API) */
   		VL53L7CX_WaitMs(&(Dev.platform), 5);
   	}

   	status = vl53l7cx_stop_ranging(&Dev);
   	printf("Stop ranging autonomous\n");

	
	/*********************************/
	/* Set ranging mode continuous   */
	/*********************************/

	/* In continuous mode, the integration time cannot be programmed
	 * (automatically set to maximum value) */

	status = vl53l7cx_set_ranging_mode(&Dev, VL53L7CX_RANGING_MODE_CONTINUOUS);
	if(status)
	{
		printf("vl53l7cx_set_ranging_mode failed, status %u\n", status);
		return status;
	}

	/* Trying to update value below will have no impact on integration time */
	//status = vl53l7cx_set_integration_time_ms(&Dev, 20);

	/* Start a ranging session */
   	status = vl53l7cx_start_ranging(&Dev);
   	printf("Start ranging continuous\n");

   	loop = 0;
   	while(loop < 10)
   	{
   		status = vl53l7cx_check_data_ready(&Dev, &isReady);
   		if(isReady)
   		{
   			vl53l7cx_get_ranging_data(&Dev, &Results);

   			/* As the sensor is set in 4x4 mode by default, we have a total
			 * of 16 zones to print */
   			printf("Print data no : %3u\n", Dev.streamcount);
   			for(i = 0; i < 16; i++)
   			{
   				printf("Zone : %3d, Status : %3u, Distance : %4d mm\n",
					i,
					Results.target_status[VL53L7CX_NB_TARGET_PER_ZONE*i],
					Results.distance_mm[VL53L7CX_NB_TARGET_PER_ZONE*i]);
   			}
   			printf("\n");
   			loop++;
   		}

		/* Wait a few ms to avoid too high polling (function in platform
		 * file, not in API) */
   		VL53L7CX_WaitMs(&(Dev.platform), 5);
   	}

   	status = vl53l7cx_stop_ranging(&Dev);
   	printf("Stop ranging continuous\n");

   	printf("End of ULD demo\n");
   	return status;
}
